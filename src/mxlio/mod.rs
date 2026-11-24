//! MXL I/O on the C API (`mxl-sys`, libmxl loaded at run time): instances per
//! domain, discrete (video) readers and writers, continuous (audio) readers
//! and writers, and TAI time.
//!
//! The `mxl` crate's wrappers hide the grain info (slices, flags, the index a
//! ring slot really holds), which a reader that holds frames needs; this is a
//! thin safe layer over the same calls.

pub mod domain;
pub mod flowdef;

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mxl_sys::{FlowConfigInfo, FlowReader, FlowRuntimeInfo, FlowWriter, GrainInfo, Instance as RawInstance, Rational};
use uuid::Uuid;

/// libmxl, found through the dynamic linker (the image installs it in the
/// linker path; locally `LD_LIBRARY_PATH`).
pub const LIBRARY: &str = "libmxl.so.1";

pub type Api = Arc<mxl::MxlApi>;

/// Load libmxl.
pub fn load() -> Result<Api, String> {
    mxl::load_api(LIBRARY).map_err(|e| format!("cannot load {LIBRARY}: {e}"))
}

/// An MXL status as an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MxlError {
    #[error("flow not found")]
    FlowNotFound,
    #[error("too late (the grain left the ring)")]
    TooLate,
    #[error("too early (the grain is not written yet)")]
    TooEarly,
    #[error("timeout")]
    Timeout,
    #[error("invalid argument")]
    InvalidArg,
    #[error("flow invalid (its writer re-created it)")]
    FlowInvalid,
    #[error("MXL status {0}")]
    Other(u32),
}

fn check(status: mxl_sys::Status) -> Result<(), MxlError> {
    match status {
        mxl_sys::MXL_STATUS_OK => Ok(()),
        mxl_sys::MXL_ERR_FLOW_NOT_FOUND => Err(MxlError::FlowNotFound),
        mxl_sys::MXL_ERR_OUT_OF_RANGE_TOO_LATE => Err(MxlError::TooLate),
        mxl_sys::MXL_ERR_OUT_OF_RANGE_TOO_EARLY => Err(MxlError::TooEarly),
        mxl_sys::MXL_ERR_TIMEOUT => Err(MxlError::Timeout),
        mxl_sys::MXL_ERR_INVALID_ARG => Err(MxlError::InvalidArg),
        mxl_sys::MXL_ERR_FLOW_INVALID => Err(MxlError::FlowInvalid),
        other => Err(MxlError::Other(other as u32)),
    }
}

/// A rate as MXL wants it.
pub fn rational(n: u32, d: u32) -> Rational {
    Rational {
        numerator: n as i64,
        denominator: d.max(1) as i64,
    }
}

/// TAI time and grain indices (libmxl's clock: `CLOCK_TAI`).
#[derive(Clone)]
pub struct Clock {
    api: Api,
}

impl Clock {
    pub fn new(api: Api) -> Self {
        Self { api }
    }

    /// TAI now, ns since the epoch.
    pub fn now(&self) -> u64 {
        unsafe { self.api.get_time() }
    }

    pub fn index_now(&self, rate: &Rational) -> u64 {
        unsafe { self.api.get_current_index(rate) }
    }

    pub fn index_to_time(&self, rate: &Rational, index: u64) -> u64 {
        unsafe { self.api.index_to_timestamp(rate, index) }
    }

    pub fn time_to_index(&self, rate: &Rational, time: u64) -> u64 {
        unsafe { self.api.timestamp_to_index(rate, time) }
    }

    /// Sleep until TAI `time` (ns).
    pub fn sleep_until(&self, time: u64) {
        unsafe { self.api.sleep_until(time) }
    }
}

/// One MXL instance on one domain directory.
pub struct Instance {
    api: Api,
    raw: RawInstance,
    pub domain: PathBuf,
}

// SAFETY: MXL instances are thread-safe; readers and writers are not (they
// are used from one thread at a time, see `VideoReader`).
unsafe impl Send for Instance {}
unsafe impl Sync for Instance {}

impl Instance {
    pub fn new(api: &Api, domain: &Path) -> Result<Arc<Instance>, String> {
        let d = CString::new(domain.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
        let o = CString::new("").unwrap();
        let raw = unsafe { api.create_instance(d.as_ptr(), o.as_ptr()) };
        if raw.is_null() {
            return Err(format!("cannot open MXL domain {}", domain.display()));
        }
        Ok(Arc::new(Instance {
            api: api.clone(),
            raw,
            domain: domain.to_path_buf(),
        }))
    }

    /// The flow definition (JSON) of a flow in this domain.
    pub fn flow_def(&self, flow: Uuid) -> Result<String, MxlError> {
        let id = CString::new(flow.to_string()).unwrap();
        let mut buf = vec![0u8; 8192];
        let mut size = buf.len();
        let status = unsafe {
            self.api
                .get_flow_def(self.raw, id.as_ptr(), buf.as_mut_ptr() as *mut _, &mut size)
        };
        if status == mxl_sys::MXL_ERR_INVALID_ARG && size > buf.len() {
            buf = vec![0u8; size];
            check(unsafe {
                self.api
                    .get_flow_def(self.raw, id.as_ptr(), buf.as_mut_ptr() as *mut _, &mut size)
            })?;
        } else {
            check(status)?;
        }
        buf.truncate(size);
        while buf.last() == Some(&0) {
            buf.pop();
        }
        String::from_utf8(buf).map_err(|_| MxlError::InvalidArg)
    }

    /// A reader of a discrete (video) flow.
    pub fn video_reader(self: &Arc<Self>, flow: Uuid) -> Result<VideoReader, MxlError> {
        let raw = self.reader(flow)?;
        Ok(VideoReader {
            instance: self.clone(),
            raw,
        })
    }

    /// A reader of a continuous (audio) flow.
    pub fn audio_reader(self: &Arc<Self>, flow: Uuid) -> Result<AudioReader, MxlError> {
        let raw = self.reader(flow)?;
        let mut info: FlowConfigInfo = unsafe { std::mem::zeroed() };
        check(unsafe { self.api.flow_reader_get_config_info(raw, &mut info) })?;
        Ok(AudioReader {
            instance: self.clone(),
            raw,
            channels: unsafe { info.__bindgen_anon_1.continuous.channelCount } as usize,
            rate: info.common.grainRate,
        })
    }

    fn reader(&self, flow: Uuid) -> Result<FlowReader, MxlError> {
        let id = CString::new(flow.to_string()).unwrap();
        let o = CString::new("").unwrap();
        let mut raw: FlowReader = std::ptr::null_mut();
        check(unsafe { self.api.create_flow_reader(self.raw, id.as_ptr(), o.as_ptr(), &mut raw) })?;
        if raw.is_null() {
            return Err(MxlError::FlowNotFound);
        }
        Ok(raw)
    }

    /// A writer of a flow (created from `flow_def` unless it exists).
    pub fn writer(self: &Arc<Self>, flow_def: &str) -> Result<(RawWriter, bool), MxlError> {
        let def = CString::new(flow_def).map_err(|_| MxlError::InvalidArg)?;
        let mut raw: FlowWriter = std::ptr::null_mut();
        let mut info: FlowConfigInfo = unsafe { std::mem::zeroed() };
        let mut created = false;
        check(unsafe {
            self.api.create_flow_writer(
                self.raw,
                def.as_ptr(),
                std::ptr::null(),
                &mut raw,
                &mut info,
                &mut created,
            )
        })?;
        if raw.is_null() {
            return Err(MxlError::InvalidArg);
        }
        Ok((
            RawWriter {
                instance: self.clone(),
                raw,
                info,
            },
            created,
        ))
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            self.api.destroy_instance(self.raw);
        }
    }
}

/// A complete grain read from a ring: a view of the ring's memory, valid
/// while its reader is open (the slot may be overwritten once the writer
/// laps it; the ring holds `history_duration` of grains).
#[derive(Clone, Copy)]
pub struct Grain {
    pub index: u64,
    pub ptr: *const u8,
    pub len: usize,
    pub flags: u32,
}

// SAFETY: a view of shared memory; see above.
unsafe impl Send for Grain {}

impl Grain {
    /// # Safety
    /// The reader must still be open and the writer not have lapped it.
    pub unsafe fn data<'a>(&self) -> &'a [u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn invalid(&self) -> bool {
        self.flags & mxl_sys::MXL_GRAIN_FLAG_INVALID != 0
    }
}

/// A discrete flow reader.
pub struct VideoReader {
    instance: Arc<Instance>,
    raw: FlowReader,
}

unsafe impl Send for VideoReader {}

impl VideoReader {
    /// The head index (the newest grain written).
    pub fn head(&self) -> Result<u64, MxlError> {
        let mut info: FlowRuntimeInfo = unsafe { std::mem::zeroed() };
        check(unsafe { self.instance.api.flow_reader_get_runtime_info(self.raw, &mut info) })?;
        Ok(info.headIndex)
    }

    pub fn grain_rate(&self) -> Result<Rational, MxlError> {
        let mut info: FlowConfigInfo = unsafe { std::mem::zeroed() };
        check(unsafe { self.instance.api.flow_reader_get_config_info(self.raw, &mut info) })?;
        Ok(info.common.grainRate)
    }

    /// Grain `index` if it is complete and in the ring now (never waits).
    /// `Ok(None)`: the slot holds another grain or this one is partial.
    pub fn grain(&self, index: u64) -> Result<Option<Grain>, MxlError> {
        let mut info: GrainInfo = unsafe { std::mem::zeroed() };
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let status = unsafe {
            self.instance.api.flow_reader_get_grain_slice_non_blocking(
                self.raw,
                index,
                u16::MAX,
                &mut info,
                &mut ptr,
            )
        };
        match check(status) {
            Ok(()) => {}
            Err(MxlError::TooEarly) | Err(MxlError::Timeout) => return Ok(None),
            Err(e) => return Err(e),
        }
        if ptr.is_null() || info.index != index || info.validSlices < info.totalSlices {
            return Ok(None);
        }
        Ok(Some(Grain {
            index,
            ptr,
            len: info.grainSize as usize,
            flags: info.flags,
        }))
    }

    /// Wait up to `timeout_ns` for grain `index` to be complete.
    pub fn wait_grain(&self, index: u64, timeout_ns: u64) -> Result<Option<Grain>, MxlError> {
        let mut info: GrainInfo = unsafe { std::mem::zeroed() };
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let status = unsafe {
            self.instance.api.flow_reader_get_grain_slice(
                self.raw,
                index,
                u16::MAX,
                timeout_ns,
                &mut info,
                &mut ptr,
            )
        };
        match check(status) {
            Ok(()) => {}
            Err(MxlError::TooEarly) | Err(MxlError::Timeout) => return Ok(None),
            Err(e) => return Err(e),
        }
        if ptr.is_null() || info.index != index || info.validSlices < info.totalSlices {
            return Ok(None);
        }
        Ok(Some(Grain {
            index,
            ptr,
            len: info.grainSize as usize,
            flags: info.flags,
        }))
    }
}

impl Drop for VideoReader {
    fn drop(&mut self) {
        unsafe {
            self.instance.api.release_flow_reader(self.instance.raw, self.raw);
        }
    }
}

/// A flow writer (video or audio).
pub struct RawWriter {
    instance: Arc<Instance>,
    raw: FlowWriter,
    pub info: FlowConfigInfo,
}

unsafe impl Send for RawWriter {}

impl RawWriter {
    pub fn id(&self) -> Uuid {
        Uuid::from_bytes(self.info.common.id)
    }

    /// Open grain `index`, let `f` fill its payload, commit it whole.
    pub fn write_grain<R>(&mut self, index: u64, f: impl FnOnce(&mut [u8]) -> R) -> Result<R, MxlError> {
        let mut info: GrainInfo = unsafe { std::mem::zeroed() };
        let mut ptr: *mut u8 = std::ptr::null_mut();
        check(unsafe { self.instance.api.flow_writer_open_grain(self.raw, index, &mut info, &mut ptr) })?;
        if ptr.is_null() {
            unsafe { self.instance.api.flow_writer_cancel_grain(self.raw) };
            return Err(MxlError::InvalidArg);
        }
        let payload = unsafe { std::slice::from_raw_parts_mut(ptr, info.grainSize as usize) };
        let r = f(payload);
        info.validSlices = info.totalSlices;
        info.flags = 0;
        check(unsafe { self.instance.api.flow_writer_commit_grain(self.raw, &info) })?;
        Ok(r)
    }

    /// Open `count` samples at `index` (the last sample), let `f` fill each
    /// channel's (up to two) fragments, commit.
    pub fn write_samples(
        &mut self,
        index: u64,
        count: usize,
        f: impl FnOnce(&mut [(&mut [f32], &mut [f32])]),
    ) -> Result<(), MxlError> {
        let mut buf: mxl_sys::MutableWrappedMultiBufferSlice = unsafe { std::mem::zeroed() };
        check(unsafe { self.instance.api.flow_writer_open_samples(self.raw, index, count, &mut buf) })?;
        let channels = buf.count;
        let mut slices: Vec<(&mut [f32], &mut [f32])> = (0..channels)
            .map(|c| unsafe {
                let frag = |k: usize| {
                    let f = &buf.base.fragments[k];
                    let ptr = (f.pointer as *mut u8).add(c * buf.stride) as *mut f32;
                    std::slice::from_raw_parts_mut(ptr, f.size / 4)
                };
                (frag(0), frag(1))
            })
            .collect();
        f(&mut slices);
        check(unsafe { self.instance.api.flow_writer_commit_samples(self.raw) })
    }
}

impl Drop for RawWriter {
    fn drop(&mut self) {
        unsafe {
            self.instance.api.release_flow_writer(self.instance.raw, self.raw);
        }
    }
}

/// A continuous (audio) flow reader.
pub struct AudioReader {
    instance: Arc<Instance>,
    raw: FlowReader,
    pub channels: usize,
    pub rate: Rational,
}

unsafe impl Send for AudioReader {}

impl AudioReader {
    pub fn head(&self) -> Result<u64, MxlError> {
        let mut info: FlowRuntimeInfo = unsafe { std::mem::zeroed() };
        check(unsafe { self.instance.api.flow_reader_get_runtime_info(self.raw, &mut info) })?;
        Ok(info.headIndex)
    }

    /// Copy `count` samples ending at `index` of each channel into `out`
    /// (`out[c]` gets `count` samples). `false`: not available.
    pub fn read(&self, index: u64, count: usize, out: &mut [Vec<f32>]) -> Result<bool, MxlError> {
        let mut buf: mxl_sys::WrappedMultiBufferSlice = unsafe { std::mem::zeroed() };
        let status = unsafe {
            self.instance
                .api
                .flow_reader_get_samples_non_blocking(self.raw, index, count, &mut buf)
        };
        match check(status) {
            Ok(()) => {}
            Err(MxlError::TooEarly) | Err(MxlError::TooLate) | Err(MxlError::Timeout) => return Ok(false),
            Err(e) => return Err(e),
        }
        for (c, o) in out.iter_mut().enumerate().take(buf.count) {
            o.clear();
            for k in 0..2 {
                let f = &buf.base.fragments[k];
                if f.size == 0 {
                    continue;
                }
                let ptr = unsafe { (f.pointer as *const u8).add(c * buf.stride) as *const f32 };
                o.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr, f.size / 4) });
            }
        }
        Ok(true)
    }
}

impl Drop for AudioReader {
    fn drop(&mut self) {
        unsafe {
            self.instance.api.release_flow_reader(self.instance.raw, self.raw);
        }
    }
}

/// A flow's directory in a domain.
pub fn flow_dir(domain: &Path, flow: Uuid) -> PathBuf {
    domain.join(format!("{flow}.mxl-flow"))
}

/// The inode of a flow's data file: it changes when the flow is deleted and
/// re-created by a restarted writer.
pub fn flow_inode(domain: &Path, flow: Uuid) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(flow_dir(domain, flow).join("data")).ok().map(|m| m.ino())
}
