//! libomt (Open Media Transport, MIT) through its C API (`libomt.h`), loaded
//! at run time with `dlopen` so that the binary, its tests and the tools run
//! without it. libomt is libomtnet compiled to native code with .NET
//! NativeAOT; it loads `libvmx.so` (the VMX codec) itself.
//!
//! Only what the bridge uses is bound: discovery, receive, send, statistics,
//! settings and logging.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::{Arc, OnceLock};

use libloading::Library;

/// libomt's soname as the image installs it.
pub const LIBRARY: &str = "libomt.so";

pub const FRAME_VIDEO: c_int = 2;
pub const FRAME_AUDIO: c_int = 4;

/// FourCCs (little endian) of `OMTCodec`.
pub mod codec {
    pub const VMX1: i32 = 0x31584D56;
    pub const FPA1: i32 = 0x31415046;
    pub const UYVY: i32 = 0x59565955;
    pub const YUY2: i32 = 0x32595559;
    pub const BGRA: i32 = 0x41524742;
    pub const NV12: i32 = 0x3231564E;
    pub const YV12: i32 = 0x32315659;
    pub const UYVA: i32 = 0x41565955;
    pub const P216: i32 = 0x36313250;
    pub const PA16: i32 = 0x36314150;

    pub fn name(c: i32) -> String {
        let b = c.to_le_bytes();
        if b.iter().all(|x| x.is_ascii_alphanumeric()) {
            String::from_utf8_lossy(&b).into_owned()
        } else {
            format!("0x{c:08x}")
        }
    }
}

pub const FLAG_INTERLACED: c_int = 1;
pub const FLAG_ALPHA: c_int = 2;
pub const FLAG_HIGH_BIT_DEPTH: c_int = 16;

/// `OMTPreferredVideoFormat_UYVYorUYVAorP216orPA16`: 8-bit sources arrive as
/// UYVY(A), high-bit-depth ones as P216/PA16.
pub const PREFER_UYVY_OR_P216: c_int = 4;
pub const PREFER_UYVY: c_int = 0;

pub const COLORSPACE_BT709: c_int = 709;

/// `OMTMediaFrame` (zero it before use).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MediaFrame {
    pub kind: c_int,
    /// 100 ns units; -1 lets the sender stamp and pace
    pub timestamp: i64,
    pub codec: c_int,
    pub width: c_int,
    pub height: c_int,
    pub stride: c_int,
    pub flags: c_int,
    pub frame_rate_n: c_int,
    pub frame_rate_d: c_int,
    pub aspect_ratio: f32,
    pub color_space: c_int,
    pub sample_rate: c_int,
    pub channels: c_int,
    pub samples_per_channel: c_int,
    pub data: *mut c_void,
    pub data_length: c_int,
    pub compressed_data: *mut c_void,
    pub compressed_length: c_int,
    pub frame_metadata: *mut c_void,
    pub frame_metadata_length: c_int,
}

impl Default for MediaFrame {
    fn default() -> Self {
        // SAFETY: plain data; all-zero is the documented initial state.
        unsafe { std::mem::zeroed() }
    }
}

/// `OMTStatistics`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct Statistics {
    pub bytes_sent: i64,
    pub bytes_received: i64,
    pub bytes_sent_since_last: i64,
    pub bytes_received_since_last: i64,
    pub frames: i64,
    pub frames_since_last: i64,
    pub frames_dropped: i64,
    /// ms spent in the codec in total
    pub codec_time: i64,
    /// ms of the last frame
    pub codec_time_since_last: i64,
    #[serde(skip)]
    pub reserved: [i64; 7],
}

/// `OMTSenderInfo`.
#[repr(C)]
pub struct SenderInfo {
    pub product_name: [c_char; 1024],
    pub manufacturer: [c_char; 1024],
    pub version: [c_char; 1024],
    pub reserved: [[c_char; 1024]; 3],
}

impl SenderInfo {
    pub fn new(product: &str, manufacturer: &str, version: &str) -> Box<Self> {
        // SAFETY: plain data.
        let mut s: Box<Self> = Box::new(unsafe { std::mem::zeroed() });
        fill(&mut s.product_name, product);
        fill(&mut s.manufacturer, manufacturer);
        fill(&mut s.version, version);
        s
    }

    pub fn product(&self) -> String {
        cstr(&self.product_name)
    }
    pub fn manufacturer_str(&self) -> String {
        cstr(&self.manufacturer)
    }
    pub fn version_str(&self) -> String {
        cstr(&self.version)
    }
}

fn fill(dst: &mut [c_char; 1024], s: &str) {
    for (d, b) in dst.iter_mut().zip(s.bytes().take(1023)) {
        *d = b as c_char;
    }
}

fn cstr(b: &[c_char; 1024]) -> String {
    let bytes: Vec<u8> = b.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

type Handle = *mut c_void;

#[allow(non_snake_case)]
struct Fns {
    discovery_getaddresses: unsafe extern "C" fn(*mut c_int) -> *mut *mut c_char,
    receive_create: unsafe extern "C" fn(*const c_char, c_int, c_int, c_int) -> Handle,
    receive_destroy: unsafe extern "C" fn(Handle),
    receive: unsafe extern "C" fn(Handle, c_int, c_int) -> *mut MediaFrame,
    receive_getsenderinformation: unsafe extern "C" fn(Handle, *mut SenderInfo),
    receive_getvideostatistics: unsafe extern "C" fn(Handle, *mut Statistics),
    receive_getaudiostatistics: unsafe extern "C" fn(Handle, *mut Statistics),
    receive_setsuggestedquality: unsafe extern "C" fn(Handle, c_int),
    send_create: unsafe extern "C" fn(*const c_char, c_int) -> Handle,
    send_setsenderinformation: unsafe extern "C" fn(Handle, *mut SenderInfo),
    send_getaddress: unsafe extern "C" fn(Handle, *mut c_char, c_int) -> c_int,
    send_destroy: unsafe extern "C" fn(Handle),
    send: unsafe extern "C" fn(Handle, *mut MediaFrame) -> c_int,
    send_connections: unsafe extern "C" fn(Handle) -> c_int,
    send_getvideostatistics: unsafe extern "C" fn(Handle, *mut Statistics),
    send_getaudiostatistics: unsafe extern "C" fn(Handle, *mut Statistics),
    setloggingfilename: unsafe extern "C" fn(*const c_char),
    settings_set_string: unsafe extern "C" fn(*const c_char, *const c_char),
    settings_get_string: unsafe extern "C" fn(*const c_char, *mut c_char, c_int) -> c_int,
    settings_set_integer: unsafe extern "C" fn(*const c_char, c_int),
    settings_get_integer: unsafe extern "C" fn(*const c_char) -> c_int,
}

/// The loaded library.
pub struct Omt {
    f: Fns,
    _lib: Library,
    discovery: std::sync::Mutex<()>,
}

pub type OmtApi = Arc<Omt>;

static LOADED: OnceLock<Result<OmtApi, String>> = OnceLock::new();

/// Load libomt once.
pub fn load() -> Result<OmtApi, String> {
    LOADED
        .get_or_init(|| unsafe {
            let lib = Library::new(LIBRARY).map_err(|e| format!("cannot load {LIBRARY}: {e}"))?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get(concat!($name, "\0").as_bytes())
                        .map_err(|e| format!("{LIBRARY}: {}: {e}", $name))?
                };
            }
            let f = Fns {
                discovery_getaddresses: sym!("omt_discovery_getaddresses"),
                receive_create: sym!("omt_receive_create"),
                receive_destroy: sym!("omt_receive_destroy"),
                receive: sym!("omt_receive"),
                receive_getsenderinformation: sym!("omt_receive_getsenderinformation"),
                receive_getvideostatistics: sym!("omt_receive_getvideostatistics"),
                receive_getaudiostatistics: sym!("omt_receive_getaudiostatistics"),
                receive_setsuggestedquality: sym!("omt_receive_setsuggestedquality"),
                send_create: sym!("omt_send_create"),
                send_setsenderinformation: sym!("omt_send_setsenderinformation"),
                send_getaddress: sym!("omt_send_getaddress"),
                send_destroy: sym!("omt_send_destroy"),
                send: sym!("omt_send"),
                send_connections: sym!("omt_send_connections"),
                send_getvideostatistics: sym!("omt_send_getvideostatistics"),
                send_getaudiostatistics: sym!("omt_send_getaudiostatistics"),
                setloggingfilename: sym!("omt_setloggingfilename"),
                settings_set_string: sym!("omt_settings_set_string"),
                settings_get_string: sym!("omt_settings_get_string"),
                settings_set_integer: sym!("omt_settings_set_integer"),
                settings_get_integer: sym!("omt_settings_get_integer"),
            };
            Ok(Arc::new(Omt {
                f,
                _lib: lib,
                discovery: std::sync::Mutex::new(()),
            }))
        })
        .clone()
}

fn c(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap()
}

impl Omt {
    /// The OMT sources discovery knows now (`HOST (Name)`).
    pub fn discover(&self) -> Vec<String> {
        // The array is valid until the next call: one caller at a time.
        let _g = self.discovery.lock().unwrap_or_else(|e| e.into_inner());
        let mut n: c_int = 0;
        let arr = unsafe { (self.f.discovery_getaddresses)(&mut n) };
        if arr.is_null() || n <= 0 {
            return Vec::new();
        }
        (0..n as usize)
            .filter_map(|i| {
                let p = unsafe { *arr.add(i) };
                (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
            })
            .collect()
    }

    pub fn set_logging_file(&self, path: Option<&str>) {
        let p = path.map(c);
        unsafe { (self.f.setloggingfilename)(p.as_ref().map_or(std::ptr::null(), |p| p.as_ptr())) }
    }

    pub fn set_string(&self, name: &str, value: &str) {
        unsafe { (self.f.settings_set_string)(c(name).as_ptr(), c(value).as_ptr()) }
    }

    pub fn get_string(&self, name: &str) -> String {
        let mut buf = vec![0 as c_char; 1024];
        let n = unsafe { (self.f.settings_get_string)(c(name).as_ptr(), buf.as_mut_ptr(), buf.len() as c_int) };
        if n <= 0 {
            return String::new();
        }
        unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
    }

    pub fn set_integer(&self, name: &str, value: i32) {
        unsafe { (self.f.settings_set_integer)(c(name).as_ptr(), value) }
    }

    pub fn get_integer(&self, name: &str) -> i32 {
        unsafe { (self.f.settings_get_integer)(c(name).as_ptr()) }
    }
}

/// A receiver (`omt_receive_*`). Used from one thread.
pub struct Receiver {
    api: OmtApi,
    h: Handle,
}

unsafe impl Send for Receiver {}

impl Receiver {
    /// Connect to `address` (`HOST (Name)` from discovery, or
    /// `omt://host:port`); it connects and reconnects in the background.
    pub fn new(api: &OmtApi, address: &str, frame_types: c_int, preferred: c_int) -> Result<Receiver, String> {
        let h = unsafe { (api.f.receive_create)(c(address).as_ptr(), frame_types, preferred, 0) };
        if h.is_null() {
            return Err(format!("omt_receive_create({address}) failed"));
        }
        Ok(Receiver { api: api.clone(), h })
    }

    /// The next frame of `types`, waiting up to `timeout_ms`. Valid until the
    /// next call for that frame type.
    pub fn receive(&mut self, types: c_int, timeout_ms: i32) -> Option<&MediaFrame> {
        let p = unsafe { (self.api.f.receive)(self.h, types, timeout_ms) };
        if p.is_null() {
            None
        } else {
            Some(unsafe { &*p })
        }
    }

    pub fn sender_info(&self) -> Option<(String, String, String)> {
        let mut info = SenderInfo::new("", "", "");
        unsafe { (self.api.f.receive_getsenderinformation)(self.h, &mut *info) };
        let p = info.product();
        (!p.is_empty()).then(|| (p, info.manufacturer_str(), info.version_str()))
    }

    pub fn video_stats(&self) -> Statistics {
        let mut s = Statistics::default();
        unsafe { (self.api.f.receive_getvideostatistics)(self.h, &mut s) };
        s
    }

    pub fn audio_stats(&self) -> Statistics {
        let mut s = Statistics::default();
        unsafe { (self.api.f.receive_getaudiostatistics)(self.h, &mut s) };
        s
    }

    pub fn suggest_quality(&self, q: c_int) {
        unsafe { (self.api.f.receive_setsuggestedquality)(self.h, q) }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        unsafe { (self.api.f.receive_destroy)(self.h) }
    }
}

/// A sender (`omt_send_*`). Sent to from one thread; statistics may be read
/// from another.
pub struct Sender {
    api: OmtApi,
    h: Handle,
}

unsafe impl Send for Sender {}
unsafe impl Sync for Sender {}

impl Sender {
    /// `quality`: 0 default (the receivers decide), 1 low, 50 medium, 100 high.
    pub fn new(api: &OmtApi, name: &str, quality: c_int) -> Result<Sender, String> {
        let h = unsafe { (api.f.send_create)(c(name).as_ptr(), quality) };
        if h.is_null() {
            return Err(format!("omt_send_create({name}) failed"));
        }
        let s = Sender { api: api.clone(), h };
        let mut info = SenderInfo::new("lm-mxl-omt", "Livemotion", env!("CARGO_PKG_VERSION"));
        unsafe { (api.f.send_setsenderinformation)(h, &mut *info) };
        Ok(s)
    }

    /// `HOST (Name)`.
    pub fn address(&self) -> String {
        let mut buf = vec![0 as c_char; 1024];
        let n = unsafe { (self.api.f.send_getaddress)(self.h, buf.as_mut_ptr(), buf.len() as c_int) };
        if n <= 0 {
            return String::new();
        }
        unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
    }

    /// Send a frame to every connected receiver. Returns the bytes sent.
    pub fn send(&self, frame: &mut MediaFrame) -> i32 {
        unsafe { (self.api.f.send)(self.h, frame) }
    }

    /// Connections (a receiver has one for video and one for audio).
    pub fn connections(&self) -> i32 {
        unsafe { (self.api.f.send_connections)(self.h) }
    }

    pub fn video_stats(&self) -> Statistics {
        let mut s = Statistics::default();
        unsafe { (self.api.f.send_getvideostatistics)(self.h, &mut s) };
        s
    }

    pub fn audio_stats(&self) -> Statistics {
        let mut s = Statistics::default();
        unsafe { (self.api.f.send_getaudiostatistics)(self.h, &mut s) };
        s
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        unsafe { (self.api.f.send_destroy)(self.h) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_libomt_h() {
        // x86-64 / aarch64 LP64 offsets of OMTMediaFrame.
        assert_eq!(std::mem::offset_of!(MediaFrame, timestamp), 8);
        assert_eq!(std::mem::offset_of!(MediaFrame, codec), 16);
        assert_eq!(std::mem::offset_of!(MediaFrame, samples_per_channel), 60);
        assert_eq!(std::mem::offset_of!(MediaFrame, data), 64);
        assert_eq!(std::mem::offset_of!(MediaFrame, data_length), 72);
        assert_eq!(std::mem::offset_of!(MediaFrame, compressed_data), 80);
        assert_eq!(std::mem::offset_of!(MediaFrame, frame_metadata), 96);
        assert_eq!(std::mem::size_of::<MediaFrame>(), 112);
        assert_eq!(std::mem::size_of::<Statistics>(), 16 * 8);
        assert_eq!(std::mem::size_of::<SenderInfo>(), 6 * 1024);
        assert_eq!(codec::name(codec::UYVY), "UYVY");
        assert_eq!(codec::name(codec::P216), "P216");
    }
}
