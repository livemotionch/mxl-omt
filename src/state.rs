//! State shared between the I/O threads, the NMOS callbacks and the web API.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use serde::Serialize;

use crate::reader::Route;

/// Status of an OMT input (OMT → MXL), refreshed by its thread.
#[derive(Debug, Clone, Default, Serialize)]
pub struct InputStatus {
    pub index: usize,
    pub source: String,
    /// no_source, connecting, ok, holding, no_signal, unsupported
    pub state: String,
    pub error: Option<String>,
    pub sender: Option<String>,
    /// Format of the last frame received
    pub codec: Option<String>,
    pub width: u32,
    pub height: u32,
    pub frame_rate: Option<String>,
    pub interlaced: bool,
    pub scaled: bool,
    pub audio_rate: u32,
    pub audio_channels: u32,
    /// Frames per second received (video) over the last second
    pub fps_in: f64,
    /// Grains written per second
    pub fps_out: f64,
    pub frames_received: u64,
    pub grains_written: u64,
    /// Frames not written: a second frame for a grain already written (a
    /// faster source), or one that came after its grain was filled
    pub dropped: u64,
    /// Grains filled with the last frame (a slower source, a late frame)
    pub repeated: u64,
    /// Grains filled with black (no source, lost too long, unsupported)
    pub black: u64,
    /// The input's delay was picked again (source clock jumped or drifted)
    pub resyncs: u64,
    pub audio_frames: u64,
    pub audio_resyncs: u64,
    /// Audio blocks of silence written (no audio from the source)
    pub audio_silence: u64,
    /// TAI arrival − the sender's timestamp (only when the sender stamps
    /// TAI, as this bridge does): network, encode and decode time
    pub transit_ms: Option<f64>,
    pub transit_max_ms: Option<f64>,
    /// MXL grain time − the sender's timestamp: the delay this input adds
    /// from the sender's frame time to the grain it writes
    pub latency_ms: Option<f64>,
    /// Time to convert a frame to v210 (ms, last second)
    pub convert_ms: f64,
    /// libomt: video Mbit/s received over the last second
    pub video_mbps: f64,
    pub audio_mbps: f64,
    /// libomt: decode time per frame (ms) and frames it dropped
    pub decode_ms: f64,
    pub omt_dropped: i64,
    pub video_flow: String,
    pub audio_flow: String,
}

/// Status of an OMT output (MXL → OMT).
#[derive(Debug, Clone, Default, Serialize)]
pub struct OutputStatus {
    pub index: usize,
    pub name: String,
    pub address: String,
    pub connections: i32,
    pub video_route: Option<Route>,
    pub audio_route: Option<Route>,
    /// The MXL reader's state (ok, holding, no_signal, no_route, ...)
    pub state: String,
    pub error: Option<String>,
    pub audio_state: String,
    pub fps: f64,
    pub frames_sent: u64,
    /// Frames sent from an older grain than wanted, held, or black
    pub late: u64,
    pub held: u64,
    pub black: u64,
    /// Ticks the output thread was more than a frame late (skipped)
    pub skipped: u64,
    pub audio_frames: u64,
    pub audio_gaps: u64,
    /// Time to convert v210 and hand the frame to libomt (encode) in ms
    pub convert_ms: f64,
    pub send_ms: f64,
    pub video_mbps: f64,
    pub audio_mbps: f64,
    pub encode_ms: f64,
    pub omt_dropped: i64,
}

pub struct InputSlot {
    /// The source to connect to (changed by the API)
    pub source: Mutex<String>,
    pub status: Mutex<InputStatus>,
}

pub struct OutputSlot {
    pub video_route: Mutex<Option<Route>>,
    pub audio_route: Mutex<Option<Route>>,
    pub revision: AtomicU64,
    pub status: Mutex<OutputStatus>,
}

pub struct Shared {
    pub stop: AtomicBool,
    pub inputs: Vec<InputSlot>,
    pub outputs: Vec<OutputSlot>,
    /// Thread CPU (% of one core) by thread name, and the process total
    pub cpu: Mutex<(f64, Vec<(String, f64)>)>,
}

impl Shared {
    pub fn new(sources: Vec<String>, outputs: usize) -> std::sync::Arc<Shared> {
        std::sync::Arc::new(Shared {
            stop: AtomicBool::new(false),
            inputs: sources
                .into_iter()
                .map(|s| InputSlot {
                    source: Mutex::new(s),
                    status: Mutex::new(InputStatus::default()),
                })
                .collect(),
            outputs: (0..outputs)
                .map(|_| OutputSlot {
                    video_route: Mutex::new(None),
                    audio_route: Mutex::new(None),
                    revision: AtomicU64::new(0),
                    status: Mutex::new(OutputStatus::default()),
                })
                .collect(),
            cpu: Mutex::new((0.0, Vec::new())),
        })
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
