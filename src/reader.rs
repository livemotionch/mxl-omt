//! From lm-mxl-mixer's `src/engine/inputs.rs` (MIT): an MXL video reader
//! for the OMT outputs. Video inputs: an IS-05 route (domain, flow) becomes a reader, and per
//! output frame the grain to draw.
//!
//! For output grain `N` the input's grain at the time of output grain
//! `N − read_offset` is wanted (a writer finishes grain `M` shortly after
//! its TAI time; one grain back is always complete). If it isn't there:
//!
//! - the writer is behind: its newest complete grain (**late**);
//! - nothing newer than what was drawn last: that again (**held**), for up
//!   to `MIXER_HOLD_MAX_S`, also when the writer stopped or its flow went
//!   away (the old reader stays open, so the memory of the last grain stays
//!   mapped, until a new reader has a grain);
//! - after that, or never seen: black (**missed**).
//!
//! The output never waits for an input (with a read offset of 0 it waits
//! up to `MIXER_INPUT_WAIT_US` for the grain of the same index).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use uuid::Uuid;

use crate::mxlio::flowdef::InputFlow;
use crate::mxlio::{self, Api, Clock, Grain, Instance, MxlError, VideoReader};

/// Where an input's flow is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Route {
    pub domain: PathBuf,
    pub flow: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InputState {
    /// Not routed
    #[default]
    NoRoute,
    /// Routed; the flow isn't there (yet)
    Waiting,
    /// The flow can't be read (format)
    Unsupported,
    /// Grains as expected
    Ok,
    /// The writer is behind or stopped: an older grain is drawn
    Holding,
    /// Held longer than the hold limit, or nothing to hold: black
    NoSignal,
}

/// Counters of an input since the start.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct InputCounters {
    pub frames: u64,
    /// An older grain than wanted (the writer was behind)
    pub late: u64,
    /// The last grain again
    pub held: u64,
    /// Nothing to draw (black)
    pub missed: u64,
    pub reopened: u64,
}

struct Open {
    reader: VideoReader,
    flow: InputFlow,
    rate: mxl_sys::Rational,
    inode: Option<u64>,
}

/// One input.
pub struct VideoInput {
    pub name: String,
    route: Option<Route>,
    open: Option<Open>,
    /// A reader that went away, kept while its last grain is held
    stale: Option<Open>,
    /// The grain drawn last and whether it is in `stale`
    last: Option<(Grain, bool)>,
    /// TAI time the last grain was new
    last_new: u64,
    retry_at: u64,
    checked_at: u64,
    pub state: InputState,
    pub error: Option<String>,
    pub counters: InputCounters,
}

/// The frame of an input to draw.
#[derive(Clone, Copy)]
pub struct InputFrame {
    pub grain: Grain,
    pub width: usize,
    pub height: usize,
    pub has_key: bool,
}

/// MXL instances by domain directory, shared by all inputs.
#[derive(Default)]
pub struct Instances {
    map: HashMap<PathBuf, Arc<Instance>>,
}

impl Instances {
    pub fn get(&mut self, api: &Api, domain: &Path) -> Result<Arc<Instance>, String> {
        if let Some(i) = self.map.get(domain) {
            return Ok(i.clone());
        }
        let i = Instance::new(api, domain)?;
        self.map.insert(domain.to_path_buf(), i.clone());
        Ok(i)
    }
}

const RETRY_NS: u64 = 1_000_000_000;
const CHECK_NS: u64 = 1_000_000_000;

impl VideoInput {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            route: None,
            open: None,
            stale: None,
            last: None,
            last_new: 0,
            retry_at: 0,
            checked_at: 0,
            state: InputState::NoRoute,
            error: None,
            counters: InputCounters::default(),
        }
    }

    pub fn route(&self) -> Option<&Route> {
        self.route.as_ref()
    }

    /// The flow's definition, once open.
    pub fn flow(&self) -> Option<&InputFlow> {
        self.open.as_ref().or(self.stale.as_ref()).map(|o| &o.flow)
    }

    /// A new route (`None`: unrouted). The current picture is held until the
    /// new flow has a grain.
    pub fn set_route(&mut self, route: Option<Route>) {
        if self.route == route {
            return;
        }
        self.route = route;
        self.retire();
        if self.route.is_none() {
            self.stale = None;
            self.last = None;
            self.state = InputState::NoRoute;
        }
        self.retry_at = 0;
    }

    /// Keep the current reader only for the held grain.
    fn retire(&mut self) {
        if let Some(o) = self.open.take() {
            if let Some((_, in_stale)) = &mut self.last {
                if !*in_stale {
                    self.stale = Some(o);
                    *in_stale = true;
                }
            }
        }
    }

    fn try_open(&mut self, api: &Api, instances: &mut Instances, now: u64) {
        let Some(route) = self.route.clone() else {
            return;
        };
        if now < self.retry_at {
            return;
        }
        self.retry_at = now + RETRY_NS;
        let result = (|| -> Result<Open, (InputState, String)> {
            let inst = instances
                .get(api, &route.domain)
                .map_err(|e| (InputState::Waiting, e))?;
            let def = inst
                .flow_def(route.flow)
                .map_err(|e| (InputState::Waiting, format!("flow {}: {e}", route.flow)))?;
            let flow = InputFlow::parse(&def).map_err(|e| (InputState::Unsupported, e))?;
            if let Some(why) = flow.unsupported() {
                return Err((InputState::Unsupported, why));
            }
            let reader = inst
                .video_reader(route.flow)
                .map_err(|e| (InputState::Waiting, format!("reader: {e}")))?;
            let rate = reader
                .grain_rate()
                .map_err(|e| (InputState::Waiting, format!("rate: {e}")))?;
            Ok(Open {
                reader,
                flow,
                rate,
                inode: mxlio::flow_inode(&route.domain, route.flow),
            })
        })();
        match result {
            Ok(o) => {
                tracing::info!(
                    "{}: reading {} ({} {}x{} {}/{})",
                    self.name,
                    route.flow,
                    o.flow.media_type,
                    o.flow.width,
                    o.flow.height,
                    o.rate.numerator,
                    o.rate.denominator
                );
                self.open = Some(o);
                self.error = None;
                if self.counters.frames > 0 {
                    self.counters.reopened += 1;
                }
            }
            Err((state, e)) => {
                if self.error.as_deref() != Some(e.as_str()) {
                    tracing::warn!("{}: {e}", self.name);
                }
                if self.last.is_none() {
                    self.state = state;
                }
                self.error = Some(e);
            }
        }
    }

    /// Once a second: a flow re-created by its writer (new inode) is opened
    /// again.
    fn check(&mut self, now: u64) {
        if now < self.checked_at + CHECK_NS {
            return;
        }
        self.checked_at = now;
        if let (Some(route), Some(o)) = (&self.route, &self.open) {
            let inode = mxlio::flow_inode(&route.domain, route.flow);
            if inode != o.inode {
                tracing::info!("{}: the flow was re-created; opening it again", self.name);
                self.retire();
                self.retry_at = 0;
            }
        }
    }

    /// The grain to draw for the output frame whose input time is `time`
    /// (TAI ns of output grain `N − read_offset`). `wait_ns`: how long the
    /// grain may be waited for (read offset 0).
    #[allow(clippy::too_many_arguments)]
    pub fn frame(
        &mut self,
        api: &Api,
        instances: &mut Instances,
        clock: &Clock,
        time: u64,
        now: u64,
        wait_ns: u64,
        hold_max_ns: u64,
    ) -> Option<InputFrame> {
        self.route.as_ref()?;
        self.check(now);
        if self.open.is_none() {
            self.try_open(api, instances, now);
        }
        self.counters.frames += 1;
        let mut new_grain = None;
        if let Some(o) = &self.open {
            let index = clock.time_to_index(&o.rate, time);
            let got = if wait_ns > 0 {
                o.reader.wait_grain(index, wait_ns)
            } else {
                o.reader.grain(index)
            };
            match got {
                Ok(Some(g)) if !g.invalid() => new_grain = Some((g, false)),
                Ok(_) => {
                    // Not there: the newest complete grain, if newer than
                    // the last one drawn.
                    if let Ok(head) = o.reader.head() {
                        let newer = self.last.as_ref().is_none_or(|(l, st)| *st || head > l.index);
                        if head < index && newer {
                            if let Ok(Some(g)) = o.reader.grain(head) {
                                if !g.invalid() {
                                    new_grain = Some((g, true));
                                }
                            }
                        }
                    }
                }
                Err(MxlError::TooLate) => {
                    // Far behind the writer (a clock offset): its head.
                    if let Ok(head) = o.reader.head() {
                        if let Ok(Some(g)) = o.reader.grain(head) {
                            new_grain = Some((g, true));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("{}: {e}; opening the flow again", self.name);
                    self.error = Some(e.to_string());
                    self.retire();
                    self.retry_at = 0;
                }
            }
        }
        if let Some((g, late)) = new_grain {
            if late {
                self.counters.late += 1;
            }
            // The stale reader is no longer needed once a grain of the
            // current one is drawn.
            self.stale = None;
            self.last = Some((g, false));
            self.last_new = now;
            self.state = if late { InputState::Holding } else { InputState::Ok };
        } else if let Some(_) = &self.last {
            if now.saturating_sub(self.last_new) > hold_max_ns {
                self.counters.missed += 1;
                self.state = InputState::NoSignal;
                return None;
            }
            self.counters.held += 1;
            self.state = InputState::Holding;
        } else {
            self.counters.missed += 1;
            if self.open.is_some() {
                self.state = InputState::NoSignal;
            }
            return None;
        }
        let (grain, in_stale) = self.last?;
        let o = if in_stale { self.stale.as_ref() } else { self.open.as_ref() }?;
        Some(InputFrame {
            grain,
            width: o.flow.width,
            height: o.flow.height,
            has_key: o.flow.has_key(),
        })
    }

    /// For inputs no output draws this frame: keep the reader open and the
    /// state current, cheaply (once a second).
    pub fn poll(&mut self, api: &Api, instances: &mut Instances, clock: &Clock, now: u64) {
        if self.route.is_none() || now < self.checked_at + CHECK_NS {
            return;
        }
        self.check(now);
        if self.open.is_none() {
            self.try_open(api, instances, now);
        }
        if let Some(o) = &self.open {
            let fresh = o.reader.head().ok().and_then(|h| {
                let t = clock.index_to_time(&o.rate, h);
                Some(now.saturating_sub(t) < 500_000_000)
            });
            self.state = match fresh {
                Some(true) => InputState::Ok,
                _ => InputState::NoSignal,
            };
        }
    }
}
