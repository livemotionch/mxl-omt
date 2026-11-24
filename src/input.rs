//! OMT → MXL: one thread per input. It receives from an OMT source (libomt
//! decodes VMX to UYVY or P216), converts to v210 and writes a video grain
//! and float32 samples into the input's MXL flows, which are always there
//! (black and silence without a source).
//!
//! Timing. OMT frames carry the sender's timestamp (100 ns units). At the
//! first frame the input picks a delay `D` = arrival − timestamp (TAI), plus
//! `OMT_IN_DELAY_MS`. A video frame stamped `ts` is written to the grain
//! whose period holds `ts + D`, and audio samples to the sample index of
//! `ts + D`. So video and audio keep the sync they had at the sender, and
//! arrival jitter doesn't move frames between grains. A sender with another
//! rate or a free-running clock comes out frame-synchronised: two frames for
//! one grain → the later one is dropped; a grain without a frame → the last
//! one is repeated. `D` is picked again (a resync) when frames keep coming a
//! grain later than their mapping, or when a delay that puts frames a
//! grain earlier would have fitted every frame of the last two seconds
//! (latency trim), or the source clock jumps.
//!
//! Loss: grains nobody wrote are filled one grain late (half a period of
//! slack) with the last frame for `OMT_IN_HOLD_MS` (`OMT_IN_LOSS=hold`),
//! then black; audio is silence when none came for 100 ms.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crate::config::{Config, LossMode};
use crate::convert::{self, Planar};
use crate::mxlio::{self, flowdef, Api, Clock, Instance, RawWriter};
use crate::omt::{self, codec, OmtApi};
use crate::state::{lock, Shared};

pub fn video_flow_id(seed: &str, i: usize) -> uuid::Uuid {
    mxl_nmos::ids::derive_id(seed, &format!("in-{}/video/mxl-flow", i + 1))
}

pub fn audio_flow_id(seed: &str, i: usize) -> uuid::Uuid {
    mxl_nmos::ids::derive_id(seed, &format!("in-{}/audio/mxl-flow", i + 1))
}

pub fn labels(config: &Config, i: usize) -> (String, String) {
    (
        format!("{} In {} Video", config.label, i + 1),
        format!("{} In {} Audio", config.label, i + 1),
    )
}

const AUDIO_RATE: (u32, u32) = (48_000, 1);
/// Audio more than this off its contiguous position is placed again.
const AUDIO_SLIP: i64 = 480;

struct Timing {
    /// TAI ns − sender ns
    delay: i64,
    /// Consecutive frames more than a grain later than their mapping
    late_run: u32,
    /// Largest transit in the current trim window, and the window's start
    win_max: i64,
    win_start: Instant,
}

/// Per-second counters for rates.
#[derive(Default)]
struct Second {
    frames_in: u64,
    grains: u64,
    convert_ns: u64,
    converts: u64,
    transit_sum: f64,
    transit_n: u64,
    transit_max: f64,
    latency_sum: f64,
}

pub struct Input {
    i: usize,
    config: Arc<Config>,
    shared: Arc<Shared>,
    omt: OmtApi,
    clock: Clock,
    rate: mxl_sys::Rational,
    arate: mxl_sys::Rational,
    period: u64,
    video: RawWriter,
    audio: RawWriter,
    channels: usize,
    w: usize,
    h: usize,
    /// The last frame in v210, and black
    held: Vec<u8>,
    black: Vec<u8>,
    have_held: bool,
    planar_in: Planar,
    planar_out: Planar,
    receiver: Option<omt::Receiver>,
    source: String,
    timing: Option<Timing>,
    last_written: u64,
    last_frame: Option<Instant>,
    next_sample: Option<u64>,
    /// Grain time − sender time of the video frames (smoothed): where the
    /// audio goes, so it lands exactly as late as the picture
    av_shift: Option<i64>,
    audio_realign: bool,
    last_audio: Option<Instant>,
    sec: Second,
    sec_start: Instant,
}

impl Input {
    pub fn new(
        api: &Api,
        omt: &OmtApi,
        domain: &Path,
        config: Arc<Config>,
        shared: Arc<Shared>,
        i: usize,
    ) -> Result<Input, String> {
        let inst = Instance::new(api, domain)?;
        let f = config.format;
        let (vl, al) = labels(&config, i);
        let vdef = flowdef::video(
            video_flow_id(&config.seed, i),
            &vl,
            &format!("OMT input {} (lm-mxl-omt)", i + 1),
            &config.nmos_label,
            &f,
            false,
        );
        let (video, _) = inst.writer(&vdef).map_err(|e| format!("{vl}: {e}"))?;
        let adef = flowdef::audio(
            audio_flow_id(&config.seed, i),
            &al,
            &format!("OMT input {} audio (lm-mxl-omt)", i + 1),
            &config.nmos_label,
            config.audio_channels,
        );
        let (audio, _) = inst.writer(&adef).map_err(|e| format!("{al}: {e}"))?;
        let (w, h) = (f.width as usize, f.height as usize);
        let mut black = vec![0u8; convert::v210_frame_bytes(w, h)];
        convert::v210_black(w, h, &mut black);
        {
            let mut st = lock(&shared.inputs[i].status);
            st.index = i + 1;
            st.video_flow = video.id().to_string();
            st.audio_flow = audio.id().to_string();
            st.state = "no_source".into();
        }
        Ok(Input {
            i,
            channels: config.audio_channels,
            rate: mxlio::rational(f.rate_n, f.rate_d),
            arate: mxlio::rational(AUDIO_RATE.0, AUDIO_RATE.1),
            period: f.frame_ns(),
            config,
            shared,
            omt: omt.clone(),
            clock: Clock::new(api.clone()),
            video,
            audio,
            w,
            h,
            held: black.clone(),
            black,
            have_held: false,
            planar_in: Planar::default(),
            planar_out: Planar::default(),
            receiver: None,
            source: String::new(),
            timing: None,
            last_written: 0,
            last_frame: None,
            next_sample: None,
            av_shift: None,
            audio_realign: false,
            last_audio: None,
            sec: Second::default(),
            sec_start: Instant::now(),
        })
    }

    pub fn run(mut self) {
        while !self.shared.stopping() {
            self.check_source();
            // Wait for a frame at most until the next grain may need filling.
            let now = self.clock.now();
            let next_fill = self.clock.index_to_time(&self.rate, self.clock.index_now(&self.rate) + 1) + self.period / 2;
            let timeout_ms = ((next_fill.saturating_sub(now)) / 1_000_000).clamp(1, 40) as i32;
            if self.receiver.is_some() {
                let got = self
                    .receiver
                    .as_mut()
                    .unwrap()
                    .receive(omt::FRAME_VIDEO | omt::FRAME_AUDIO, timeout_ms)
                    .copied();
                if let Some(frame) = got {
                    match frame.kind {
                        omt::FRAME_VIDEO => self.on_video(&frame),
                        omt::FRAME_AUDIO => self.on_audio(&frame),
                        _ => {}
                    }
                }
            } else {
                std::thread::sleep(std::time::Duration::from_millis(timeout_ms as u64));
            }
            self.fill();
            if self.sec_start.elapsed().as_secs_f64() >= 1.0 {
                self.publish();
            }
        }
    }

    fn check_source(&mut self) {
        let want = lock(&self.shared.inputs[self.i].source).clone();
        if want == self.source {
            return;
        }
        tracing::info!("in-{}: source {:?}", self.i + 1, want);
        self.receiver = None;
        self.source = want.clone();
        self.timing = None;
        self.next_sample = None;
        self.av_shift = None;
        {
            let mut st = lock(&self.shared.inputs[self.i].status);
            st.source = want.clone();
            st.error = None;
            st.sender = None;
            st.state = if want.is_empty() { "no_source" } else { "connecting" }.into();
        }
        if want.is_empty() {
            return;
        }
        match omt::Receiver::new(
            &self.omt,
            &want,
            omt::FRAME_VIDEO | omt::FRAME_AUDIO,
            omt::PREFER_UYVY_OR_P216,
        ) {
            Ok(r) => {
                if self.config.in_quality > 0 {
                    r.suggest_quality(self.config.in_quality);
                }
                self.receiver = Some(r);
            }
            Err(e) => lock(&self.shared.inputs[self.i].status).error = Some(e),
        }
    }

    fn set_error(&self, e: Option<String>) {
        let mut st = lock(&self.shared.inputs[self.i].status);
        if st.error != e {
            if let Some(e) = &e {
                tracing::warn!("in-{}: {e}", self.i + 1);
            }
            st.error = e;
        }
    }

    /// Map a sender timestamp to TAI, picking or adjusting the delay.
    fn map(&mut self, ts: i64, arrival: u64, video: bool) -> u64 {
        let ts_ns = ts.saturating_mul(100);
        let transit = (arrival as i64).saturating_sub(ts_ns);
        let p = self.period as i64;
        let extra = self.config.in_delay_ns as i64;
        let mut resync = false;
        match &mut self.timing {
            None => {
                self.timing = Some(Timing {
                    delay: transit + extra,
                    late_run: 0,
                    win_max: transit,
                    win_start: Instant::now(),
                });
            }
            Some(t) if video => {
                let late = transit + extra - t.delay;
                // The source clock jumped (or restarted): place again now.
                if late.abs() > 4 * p {
                    t.delay = transit + extra;
                    resync = true;
                } else if late > p {
                    t.late_run += 1;
                    if t.late_run >= 3 {
                        t.delay = transit + extra;
                        t.late_run = 0;
                        resync = true;
                    }
                } else {
                    t.late_run = 0;
                }
                t.win_max = t.win_max.max(transit);
                if t.win_start.elapsed().as_secs() >= 2 {
                    // Trim when the slowest frame of the window would still
                    // land in time with a delay that puts this frame a
                    // grain earlier (a delay picked in a startup burst).
                    let shorter = t.win_max + extra;
                    if shorter < t.delay && (ts_ns + shorter).div_euclid(p) < (ts_ns + t.delay).div_euclid(p) {
                        t.delay = shorter;
                        resync = true;
                    }
                    t.win_max = transit;
                    t.win_start = Instant::now();
                }
            }
            Some(_) => {}
        }
        if resync {
            self.av_shift = None;
            lock(&self.shared.inputs[self.i].status).resyncs += 1;
        }
        let d = self.timing.as_ref().unwrap().delay;
        // Plausible TAI stamps (this bridge's senders): report the transit.
        if video && transit.abs() < 10_000_000_000 {
            let ms = transit as f64 / 1e6;
            self.sec.transit_sum += ms;
            self.sec.transit_n += 1;
            self.sec.transit_max = self.sec.transit_max.max(ms);
        }
        (ts_ns + d).max(0) as u64
    }

    fn on_video(&mut self, f: &omt::MediaFrame) {
        let arrival = self.clock.now();
        self.sec.frames_in += 1;
        let (fw, fh) = (f.width.max(0) as usize, f.height.max(0) as usize);
        {
            let mut st = lock(&self.shared.inputs[self.i].status);
            st.frames_received += 1;
            st.codec = Some(codec::name(f.codec));
            st.width = fw as u32;
            st.height = fh as u32;
            st.frame_rate = Some(if f.frame_rate_d <= 1 {
                format!("{}", f.frame_rate_n)
            } else {
                format!("{:.2}", f.frame_rate_n as f64 / f.frame_rate_d as f64)
            });
            st.interlaced = f.flags & omt::FLAG_INTERLACED != 0;
            st.video_ts_phase_ms = (f.timestamp.saturating_mul(100)).rem_euclid(self.period as i64) as f64 / 1e6;
            if st.sender.is_none() {
                st.sender = self.receiver.as_ref().and_then(|r| r.sender_info()).map(|(p, m, v)| format!("{m} {p} {v}"));
            }
        }
        if f.data.is_null() || f.data_length <= 0 || fw == 0 || fh == 0 {
            return;
        }
        let target = self.map(f.timestamp, arrival, true);
        let mut j = self.clock.time_to_index(&self.rate, target);
        let now_idx = self.clock.index_now(&self.rate);
        if j + 3 < now_idx || j > now_idx + 3 {
            // Mapping far off (a delay picked from a stalled first frame):
            // place this frame now and pick the delay again.
            if let Some(t) = &mut self.timing {
                t.delay += (now_idx as i64 - j as i64) * self.period as i64;
            }
            self.av_shift = None;
            lock(&self.shared.inputs[self.i].status).resyncs += 1;
            j = now_idx;
        }
        if j <= self.last_written {
            // A second frame for a grain already written, or too late.
            lock(&self.shared.inputs[self.i].status).dropped += 1;
            return;
        }
        // Convert to v210 (into `held`, which is also what a repeat writes).
        let t0 = Instant::now();
        let data = unsafe { std::slice::from_raw_parts(f.data as *const u8, f.data_length as usize) };
        let stride = f.stride.max(0) as usize;
        let p216 = f.codec == codec::P216 || f.codec == codec::PA16;
        let uyvy = f.codec == codec::UYVY || f.codec == codec::UYVA;
        let need = if p216 { stride * fh * 2 } else { stride * fh };
        let mut scaled = false;
        let ok = if !(p216 || uyvy) {
            self.set_error(Some(format!("OMT format {} is not supported (UYVY, P216)", codec::name(f.codec))));
            false
        } else if data.len() < need || stride < 2 * fw {
            self.set_error(Some(format!("short frame: {} bytes for {fw}x{fh} stride {stride}", data.len())));
            false
        } else if (fw, fh) == (self.w, self.h) {
            if p216 {
                convert::p216_to_v210(data, stride, fw, fh, &mut self.held);
            } else {
                convert::uyvy_to_v210(data, stride, fw, fh, &mut self.held);
            }
            true
        } else if self.config.in_scale {
            self.planar_in.load(data, stride, fw, fh, p216);
            self.planar_in.scale_into(&mut self.planar_out, self.w, self.h);
            self.planar_out.to_v210(&mut self.held);
            scaled = true;
            true
        } else {
            self.set_error(Some(format!(
                "{fw}x{fh} is not the house format {}x{} (OMT_IN_SCALE=false)",
                self.w, self.h
            )));
            false
        };
        self.sec.convert_ns += t0.elapsed().as_nanos() as u64;
        self.sec.converts += 1;
        if !ok {
            self.have_held = false;
            lock(&self.shared.inputs[self.i].status).state = "unsupported".into();
            return;
        }
        if lock(&self.shared.inputs[self.i].status).error.is_some() {
            self.set_error(None);
        }
        lock(&self.shared.inputs[self.i].status).scaled = scaled;
        // Grains between the last written and this one that the filler
        // didn't reach yet: the previous frame (still in `held` only if
        // the conversion went elsewhere, so this writes the new one).
        if self.last_written > 0 && j > self.last_written + 1 && j - self.last_written <= 3 {
            for k in self.last_written + 1..j {
                self.write_video(k, false);
                lock(&self.shared.inputs[self.i].status).repeated += 1;
            }
        }
        self.have_held = true;
        self.last_frame = Some(Instant::now());
        self.write_video(j, false);
        let t_j = self.clock.index_to_time(&self.rate, j) as i64;
        let shift = t_j - f.timestamp.saturating_mul(100);
        // Frame-synchronised sources shift by up to a grain from frame to
        // frame: the audio follows the average.
        self.av_shift = Some(match self.av_shift {
            None => {
                // Audio written so far went by the raw delay: place again.
                self.audio_realign = true;
                shift
            }
            Some(a) => a + (shift - a) / 16,
        });
        let lat = shift as f64 / 1e6;
        if lat.abs() < 10_000.0 {
            self.sec.latency_sum += lat;
        }
        let mut st = lock(&self.shared.inputs[self.i].status);
        st.state = "ok".into();
    }

    fn write_video(&mut self, index: u64, black: bool) {
        let src = if black { &self.black } else { &self.held };
        match self.video.write_grain(index, |p| {
            let n = p.len().min(src.len());
            p[..n].copy_from_slice(&src[..n]);
        }) {
            Ok(()) => {
                self.last_written = index;
                self.sec.grains += 1;
                let mut st = lock(&self.shared.inputs[self.i].status);
                st.grains_written += 1;
                if black {
                    st.black += 1;
                }
            }
            Err(e) => self.set_error(Some(format!("MXL video write {index}: {e}"))),
        }
    }

    fn on_audio(&mut self, f: &omt::MediaFrame) {
        let arrival = self.clock.now();
        {
            let mut st = lock(&self.shared.inputs[self.i].status);
            st.audio_frames += 1;
            st.audio_rate = f.sample_rate.max(0) as u32;
            st.audio_channels = f.channels.max(0) as u32;
            st.audio_samples = f.samples_per_channel.max(0) as u32;
            st.audio_ts_phase_ms = (f.timestamp.saturating_mul(100)).rem_euclid(self.period as i64) as f64 / 1e6;
        }
        if f.codec != codec::FPA1 || f.sample_rate != 48_000 || f.data.is_null() || f.samples_per_channel <= 0 {
            return;
        }
        let n = f.samples_per_channel as usize;
        let ch = f.channels.max(0) as usize;
        if (f.data_length as usize) < n * ch * 4 {
            return;
        }
        let mut target = self.map(f.timestamp, arrival, false);
        if let Some(shift) = self.av_shift {
            target = (f.timestamp.saturating_mul(100) + shift).max(0) as u64;
        }
        let s0 = self.clock.time_to_index(&self.arate, target);
        let data = unsafe { std::slice::from_raw_parts(f.data as *const f32, n * ch) };
        let mut skip = 0usize;
        let slip = if std::mem::take(&mut self.audio_realign) { 0 } else { AUDIO_SLIP };
        let start = match self.next_sample {
            Some(next) if (s0 as i64 - next as i64).abs() <= slip => next,
            Some(next) if s0 > next && s0 - next < 48_000 => {
                // A gap: silence up to the new position.
                lock(&self.shared.inputs[self.i].status).audio_resyncs += 1;
                self.write_silence(next, (s0 - next) as usize);
                s0
            }
            Some(next) if s0 < next => {
                // Before what is written (MXL writes only forward): the
                // part after it, if any.
                lock(&self.shared.inputs[self.i].status).audio_resyncs += 1;
                if next - s0 >= n as u64 {
                    return;
                }
                skip = (next - s0) as usize;
                next
            }
            Some(_) => {
                // Far ahead (more than a second): start there.
                lock(&self.shared.inputs[self.i].status).audio_resyncs += 1;
                s0
            }
            None => s0,
        };
        let count = n - skip;
        if count == 0 {
            return;
        }
        let channels = self.channels;
        let r = self.audio.write_samples(start + count as u64, count, |slices| {
            for (c, (a, b)) in slices.iter_mut().enumerate().take(channels) {
                let src: &[f32] = if c < ch { &data[c * n + skip..(c + 1) * n] } else { &[] };
                let la = a.len();
                for (k, v) in a.iter_mut().enumerate() {
                    *v = src.get(k).copied().unwrap_or(0.0);
                }
                for (k, v) in b.iter_mut().enumerate() {
                    *v = src.get(la + k).copied().unwrap_or(0.0);
                }
            }
        });
        match r {
            Ok(()) => {
                self.next_sample = Some(start + count as u64);
                self.last_audio = Some(Instant::now());
            }
            Err(e) => self.set_error(Some(format!("MXL audio write at {start}+{count}: {e}"))),
        }
    }

    fn write_silence(&mut self, start: u64, count: usize) {
        let mut done = 0usize;
        while done < count {
            let c = (count - done).min(1920);
            let end = start + (done + c) as u64;
            if let Err(e) = self.audio.write_samples(end, c, |slices| {
                for (a, b) in slices.iter_mut() {
                    a.fill(0.0);
                    b.fill(0.0);
                }
            }) {
                self.set_error(Some(format!("MXL audio write: {e}")));
                return;
            }
            done += c;
        }
        self.next_sample = Some(start + count as u64);
        lock(&self.shared.inputs[self.i].status).audio_silence += 1;
    }

    /// Fill grains (and audio) nobody wrote.
    fn fill(&mut self) {
        let now = self.clock.now();
        let now_idx = self.clock.index_now(&self.rate);
        // Grain k is filled once TAI passed t(k+1) + half a period.
        let t_now_idx = self.clock.index_to_time(&self.rate, now_idx);
        let due = if now >= t_now_idx + self.period / 2 { now_idx.saturating_sub(1) } else { now_idx.saturating_sub(2) };
        if self.last_written + 8 < due {
            self.last_written = due.saturating_sub(1);
        }
        while self.last_written < due {
            let k = self.last_written + 1;
            let hold = self.have_held
                && self.config.in_loss == LossMode::Hold
                && self.last_frame.is_some_and(|t| t.elapsed() < self.config.in_hold);
            self.write_video(k, !hold);
            let mut st = lock(&self.shared.inputs[self.i].status);
            if hold {
                st.repeated += 1;
                if self.last_frame.is_some_and(|t| t.elapsed().as_millis() > 3 * self.period as u128 / 1_000_000) {
                    st.state = "holding".into();
                }
            } else if !self.source.is_empty() && st.state != "unsupported" {
                st.state = if self.last_frame.is_some() || st.frames_received > 0 {
                    "no_signal"
                } else {
                    "connecting"
                }
                .into();
            }
        }
        // Audio: silence when none came for 100 ms, up to a grain ago.
        let quiet = self.last_audio.is_none_or(|t| t.elapsed().as_millis() > 100);
        if quiet {
            let end = self.clock.time_to_index(&self.arate, now.saturating_sub(self.period));
            let start = match self.next_sample {
                Some(n) if n < end && end - n < 48_000 => n,
                Some(n) if n >= end => return,
                _ => end.saturating_sub(960),
            };
            if end > start {
                self.write_silence(start, (end - start) as usize);
            }
        }
    }

    fn publish(&mut self) {
        let secs = self.sec_start.elapsed().as_secs_f64();
        let s = std::mem::take(&mut self.sec);
        self.sec_start = Instant::now();
        let (vs, as_) = match &self.receiver {
            Some(r) => (Some(r.video_stats()), Some(r.audio_stats())),
            None => (None, None),
        };
        let mut st = lock(&self.shared.inputs[self.i].status);
        st.fps_in = s.frames_in as f64 / secs;
        st.fps_out = s.grains as f64 / secs;
        st.convert_ms = if s.converts > 0 { s.convert_ns as f64 / s.converts as f64 / 1e6 } else { 0.0 };
        if s.transit_n > 0 {
            st.transit_ms = Some(s.transit_sum / s.transit_n as f64);
            st.transit_max_ms = Some(s.transit_max);
            st.latency_ms = Some(s.latency_sum / s.transit_n as f64);
        } else {
            st.transit_ms = None;
            st.transit_max_ms = None;
            st.latency_ms = None;
        }
        if let Some(v) = vs {
            st.video_mbps = v.bytes_received_since_last as f64 * 8.0 / 1e6 / secs;
            // libomt's codec time "since last" is ms over the interval.
            st.decode_ms = if v.frames_since_last > 0 {
                v.codec_time_since_last as f64 / v.frames_since_last as f64
            } else {
                0.0
            };
            st.omt_dropped = v.frames_dropped;
        }
        if let Some(a) = as_ {
            st.audio_mbps = a.bytes_received_since_last as f64 * 8.0 / 1e6 / secs;
        }
    }
}
