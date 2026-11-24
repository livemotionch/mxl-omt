//! MXL → OMT: one thread per output, one OMT sender each. Per grain `N` of
//! the house rate, at TAI `t(N)`, it reads the video grain of
//! `t(N − OMT_OUT_READ_OFFSET_GRAINS)` (a writer finishes grain `M` shortly
//! after `t(M)`), converts v210 to UYVY or P216, and sends it stamped with
//! that grain's TAI time (100 ns units), then the audio samples of the same
//! grain period, stamped alike. A receiver (this bridge's OMT input) can so
//! keep A/V sync and measure the latency.
//!
//! The MXL side is the vision mixer's input reader (`reader.rs`): a stalled
//! or re-created flow holds the last grain for `OMT_OUT_HOLD_MS`, then black.
//! Without a route the output sends black and silence (`OMT_OUT_IDLE_BLACK`),
//! so the OMT source stays up.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use crate::config::{Config, OutPixel};
use crate::convert;
use crate::mxlio::{self, Api, AudioReader, Clock};
use crate::omt::{self, codec, OmtApi};
use crate::reader::{Instances, Route, VideoInput};
use crate::state::{lock, Shared};

pub fn labels(config: &Config, i: usize) -> (String, String) {
    (
        format!("{} Out {} Video", config.label, i + 1),
        format!("{} Out {} Audio", config.label, i + 1),
    )
}

#[derive(Default)]
struct Second {
    frames: u64,
    convert_ns: u64,
    send_ns: u64,
}

pub struct Output {
    i: usize,
    config: Arc<Config>,
    shared: Arc<Shared>,
    api: Api,
    clock: Clock,
    sender: omt::Sender,
    rate: mxl_sys::Rational,
    arate: mxl_sys::Rational,
    period: u64,
    video: VideoInput,
    instances: Instances,
    audio_route: Option<Route>,
    audio: Option<AudioReader>,
    audio_retry: u64,
    audio_buf: Vec<Vec<f32>>,
    planar_audio: Vec<f32>,
    frame_buf: Vec<u8>,
    black: Vec<u8>,
    black_size: (usize, usize),
    revision: u64,
    sec: Second,
    sec_start: Instant,
}

impl Output {
    pub fn new(api: &Api, omt: &OmtApi, config: Arc<Config>, shared: Arc<Shared>, i: usize) -> Result<Output, String> {
        let name = config.outputs[i].name.clone();
        let sender = omt::Sender::new(omt, &name, config.out_quality)?;
        let address = sender.address();
        tracing::info!("out-{}: OMT sender {address:?}", i + 1);
        {
            let mut st = lock(&shared.outputs[i].status);
            st.index = i + 1;
            st.name = name;
            st.address = address;
            st.state = "no_route".into();
        }
        let f = config.format;
        Ok(Output {
            i,
            rate: mxlio::rational(f.rate_n, f.rate_d),
            arate: mxlio::rational(48_000, 1),
            period: f.frame_ns(),
            config,
            shared,
            api: api.clone(),
            clock: Clock::new(api.clone()),
            sender,
            video: VideoInput::new(format!("out-{}", i + 1)),
            instances: Instances::default(),
            audio_route: None,
            audio: None,
            audio_retry: 0,
            audio_buf: Vec::new(),
            planar_audio: Vec::new(),
            frame_buf: Vec::new(),
            black: Vec::new(),
            black_size: (0, 0),
            revision: u64::MAX,
            sec: Second::default(),
            sec_start: Instant::now(),
        })
    }

    fn routes(&mut self) {
        let slot = &self.shared.outputs[self.i];
        let rev = slot.revision.load(Ordering::Acquire);
        if rev == self.revision {
            return;
        }
        self.revision = rev;
        let v = lock(&slot.video_route).clone();
        let a = lock(&slot.audio_route).clone();
        self.video.set_route(v.clone());
        if a != self.audio_route {
            self.audio_route = a.clone();
            self.audio = None;
            self.audio_retry = 0;
        }
        let mut st = lock(&slot.status);
        st.video_route = v;
        st.audio_route = a;
    }

    pub fn run(mut self) {
        let mut n = self.clock.index_now(&self.rate) + 1;
        while !self.shared.stopping() {
            let t = self.clock.index_to_time(&self.rate, n);
            self.clock.sleep_until(t + 500_000);
            let now = self.clock.now();
            if now > t + 2 * self.period {
                // Fell behind (a stall): skip to now.
                let behind = self.clock.index_now(&self.rate);
                lock(&self.shared.outputs[self.i].status).skipped += behind - n;
                n = behind;
                continue;
            }
            self.routes();
            let src_idx = n.saturating_sub(self.config.out_read_offset);
            let src_t = self.clock.index_to_time(&self.rate, src_idx);
            self.video_frame(src_t, now);
            self.audio_frame(src_t);
            n += 1;
            if self.sec_start.elapsed().as_secs_f64() >= 1.0 {
                self.publish();
            }
        }
    }

    fn video_frame(&mut self, src_t: u64, now: u64) {
        let (w, h, out_codec) = (
            self.config.format.width as usize,
            self.config.format.height as usize,
            match self.config.out_pixel {
                OutPixel::Uyvy => codec::UYVY,
                OutPixel::P216 => codec::P216,
            },
        );
        let frame = self.video.frame(
            &self.api,
            &mut self.instances,
            &self.clock,
            src_t,
            now,
            0,
            self.config.out_hold_max.as_nanos() as u64,
        );
        let t0 = Instant::now();
        let (fw, fh) = match &frame {
            Some(f) => (f.width, f.height),
            None => (w, h),
        };
        let bpp = if out_codec == codec::P216 { 4 } else { 2 };
        let size = fw * fh * bpp;
        let data: &[u8] = match &frame {
            Some(f) => {
                // SAFETY: the reader stays open while the grain is used.
                let g = unsafe { f.grain.data() };
                if g.len() < convert::v210_frame_bytes(fw, fh) {
                    return;
                }
                self.frame_buf.resize(size, 0);
                if out_codec == codec::P216 {
                    convert::v210_to_p216(g, fw, fh, &mut self.frame_buf);
                } else {
                    convert::v210_to_uyvy(g, fw, fh, &mut self.frame_buf);
                }
                &self.frame_buf
            }
            None => {
                let mut st = lock(&self.shared.outputs[self.i].status);
                st.black += 1;
                drop(st);
                if !self.config.out_idle_black {
                    return;
                }
                if self.black_size != (fw, fh) || self.black.len() != size {
                    let mut v = vec![0u8; convert::v210_frame_bytes(fw, fh)];
                    convert::v210_black(fw, fh, &mut v);
                    self.black.resize(size, 0);
                    if out_codec == codec::P216 {
                        convert::v210_to_p216(&v, fw, fh, &mut self.black);
                    } else {
                        convert::v210_to_uyvy(&v, fw, fh, &mut self.black);
                    }
                    self.black_size = (fw, fh);
                }
                &self.black
            }
        };
        let t1 = Instant::now();
        let f = self.config.format;
        let mut mf = omt::MediaFrame {
            kind: omt::FRAME_VIDEO,
            timestamp: (src_t / 100) as i64,
            codec: out_codec,
            width: fw as i32,
            height: fh as i32,
            stride: (fw * 2) as i32,
            flags: if out_codec == codec::P216 { omt::FLAG_HIGH_BIT_DEPTH } else { 0 },
            frame_rate_n: f.rate_n as i32,
            frame_rate_d: f.rate_d as i32,
            aspect_ratio: fw as f32 / fh.max(1) as f32,
            color_space: omt::COLORSPACE_BT709,
            data: data.as_ptr() as *mut _,
            data_length: size as i32,
            ..Default::default()
        };
        self.sender.send(&mut mf);
        self.sec.convert_ns += (t1 - t0).as_nanos() as u64;
        self.sec.send_ns += t1.elapsed().as_nanos() as u64;
        self.sec.frames += 1;
        let mut st = lock(&self.shared.outputs[self.i].status);
        st.frames_sent += 1;
        st.state = serde_json::to_value(self.video.state)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        st.error = self.video.error.clone();
        st.late = self.video.counters.late;
        st.held = self.video.counters.held;
    }

    fn audio_frame(&mut self, src_t: u64) {
        let now = self.clock.now();
        if self.audio.is_none() && now >= self.audio_retry {
            if let Some(route) = self.audio_route.clone() {
                self.audio_retry = now + 1_000_000_000;
                let r = self
                    .instances
                    .get(&self.api, &route.domain)
                    .and_then(|i| i.audio_reader(route.flow).map_err(|e| e.to_string()));
                let mut st = lock(&self.shared.outputs[self.i].status);
                match r {
                    Ok(r) if r.rate.numerator == 48_000 && r.rate.denominator == 1 => {
                        tracing::info!("out-{}: reading audio {} ({} channels)", self.i + 1, route.flow, r.channels);
                        self.audio = Some(r);
                        st.audio_state = "ok".into();
                    }
                    Ok(r) => st.audio_state = format!("unsupported: {} Hz (48 kHz only)", r.rate.numerator),
                    Err(e) => st.audio_state = format!("waiting: {e}"),
                }
            } else {
                lock(&self.shared.outputs[self.i].status).audio_state = "no_route".into();
            }
        }
        let s0 = self.clock.time_to_index(&self.arate, src_t);
        let s1 = self.clock.time_to_index(&self.arate, src_t + self.period);
        let count = (s1 - s0) as usize;
        if count == 0 || count > 48_000 {
            return;
        }
        let channels = self.audio.as_ref().map_or(self.config.audio_channels, |r| r.channels).max(1);
        self.audio_buf.resize(channels, Vec::new());
        let mut ok = false;
        if let Some(r) = &self.audio {
            // The writer may finish this period's samples a little after
            // the video grain: wait for them up to 5 ms.
            for attempt in 0..6 {
                match r.read(s1, count, &mut self.audio_buf) {
                    Ok(true) => {
                        ok = true;
                        break;
                    }
                    Ok(false) => {
                        if attempt < 5 {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                    }
                    Err(e) => {
                        lock(&self.shared.outputs[self.i].status).audio_state = format!("error: {e}");
                        self.audio = None;
                        break;
                    }
                }
            }
            if !ok {
                lock(&self.shared.outputs[self.i].status).audio_gaps += 1;
            }
        } else if !self.config.out_idle_black {
            return;
        }
        self.planar_audio.clear();
        self.planar_audio.resize(channels * count, 0.0);
        if ok {
            for (c, b) in self.audio_buf.iter().enumerate() {
                let n = b.len().min(count);
                self.planar_audio[c * count..c * count + n].copy_from_slice(&b[..n]);
            }
        }
        let mut mf = omt::MediaFrame {
            kind: omt::FRAME_AUDIO,
            timestamp: (src_t / 100) as i64,
            codec: codec::FPA1,
            sample_rate: 48_000,
            channels: channels as i32,
            samples_per_channel: count as i32,
            data: self.planar_audio.as_mut_ptr() as *mut _,
            data_length: (channels * count * 4) as i32,
            ..Default::default()
        };
        self.sender.send(&mut mf);
        lock(&self.shared.outputs[self.i].status).audio_frames += 1;
    }

    fn publish(&mut self) {
        let secs = self.sec_start.elapsed().as_secs_f64();
        let s = std::mem::take(&mut self.sec);
        self.sec_start = Instant::now();
        let (v, a) = (self.sender.video_stats(), self.sender.audio_stats());
        let mut st = lock(&self.shared.outputs[self.i].status);
        st.fps = s.frames as f64 / secs;
        if s.frames > 0 {
            st.convert_ms = s.convert_ns as f64 / s.frames as f64 / 1e6;
            st.send_ms = s.send_ns as f64 / s.frames as f64 / 1e6;
        }
        st.connections = self.sender.connections();
        st.video_mbps = v.bytes_sent_since_last as f64 * 8.0 / 1e6 / secs;
        st.audio_mbps = a.bytes_sent_since_last as f64 * 8.0 / 1e6 / secs;
        st.encode_ms = v.codec_time_since_last as f64;
        st.omt_dropped = v.frames_dropped;
        if st.address.is_empty() {
            st.address = self.sender.address();
        }
    }
}
