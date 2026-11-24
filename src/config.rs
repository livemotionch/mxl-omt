//! Configuration from the environment (the mxl-stack conventions; see the
//! README's table). Invalid values are an error: the process exits with 78.

use std::path::PathBuf;
use std::time::Duration;

use uuid::Uuid;

use crate::mxlio::flowdef::VideoFormat;

/// What an OMT input writes while its source is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LossMode {
    /// The last frame for `OMT_IN_HOLD_MS`, then black
    Hold,
    /// Black at once
    Black,
}

/// The uncompressed format handed to OMT for sending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutPixel {
    /// 8-bit 4:2:2 (v210 rounded to 8 bits)
    Uyvy,
    /// 16-bit planar 4:2:2: all 10 bits, high-bit-depth VMX
    P216,
}

#[derive(Debug, Clone)]
pub struct InputConfig {
    /// OMT address (`HOST (Name)` or `omt://host:port`); empty: not connected
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct OutputConfig {
    /// OMT source name (the address becomes `HOST (name)`)
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub format: VideoFormat,
    pub inputs: Vec<InputConfig>,
    pub outputs: Vec<OutputConfig>,
    /// Prefix of every NMOS label of this instance (`OMT` → `OMT In 1 Video`)
    pub label: String,
    pub nmos_label: String,
    pub nmos_port: u16,
    pub web_port: u16,
    pub output_domain_dir: PathBuf,
    pub scan_path: PathBuf,
    pub output_domain_id: Uuid,
    pub seed: String,
    pub cleanup_on_exit: bool,
    pub history_ns: u64,
    pub state_dir: PathBuf,
    pub shutdown_timeout: Duration,
    pub audio_channels: usize,
    // OMT in
    pub in_loss: LossMode,
    pub in_hold: Duration,
    /// Added to the delay every input picks at the first frame
    pub in_delay_ns: u64,
    pub in_scale: bool,
    /// `OMTQuality` suggested by the inputs to their senders (0: no suggestion)
    pub in_quality: i32,
    // OMT out
    pub out_pixel: OutPixel,
    pub out_quality: i32,
    pub out_read_offset: u64,
    pub out_hold_max: Duration,
    /// Send black and silence while an output has no route or signal
    pub out_idle_black: bool,
    // libomt
    pub omt_discovery_server: Option<String>,
    pub omt_port_start: Option<i32>,
    pub omt_port_end: Option<i32>,
    pub omt_log_file: Option<String>,
    pub log_level: String,
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn parse<T: std::str::FromStr>(name: &str, default: T) -> Result<T, String> {
    match var(name) {
        None => Ok(default),
        Some(v) => v.parse().map_err(|_| format!("{name}={v:?} is not valid")),
    }
}

fn boolean(name: &str, default: bool) -> Result<bool, String> {
    match var(name).as_deref().map(str::to_ascii_lowercase).as_deref() {
        None => Ok(default),
        Some("1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        Some(v) => Err(format!("{name}={v:?}: expected true or false")),
    }
}

fn ranged(name: &str, default: usize, lo: usize, hi: usize) -> Result<usize, String> {
    let v: usize = parse(name, default)?;
    if v < lo || v > hi {
        return Err(format!("{name}={v}: must be {lo}–{hi}"));
    }
    Ok(v)
}

fn quality(name: &str, default: &str) -> Result<i32, String> {
    match var(name).unwrap_or_else(|| default.into()).to_ascii_lowercase().as_str() {
        "default" | "auto" => Ok(0),
        "low" => Ok(1),
        "medium" => Ok(50),
        "high" => Ok(100),
        v => Err(format!("{name}={v:?}: expected default, low, medium or high")),
    }
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        let format = VideoFormat::parse(&var("VIDEO_FORMAT").unwrap_or_else(|| "1080p50".into()))?;
        let scan_path = PathBuf::from(var("MXL_DOMAIN_SCAN_PATH").unwrap_or_else(|| "/Volumes/mxl".into()));
        let output_domain_dir = PathBuf::from(
            var("MXL_OUTPUT_DOMAIN_DIR").unwrap_or_else(|| scan_path.join("omt-bridge").to_string_lossy().into()),
        );
        let seed = var("NMOS_SEED").unwrap_or_else(|| "lm-mxl-omt".into());
        let output_domain_id = mxl_nmos::ids::derive_id(&seed, "mxl-output-domain");
        let n_in = ranged("OMT_INPUTS", 1, 0, 32)?;
        let n_out = ranged("OMT_OUTPUTS", 1, 0, 32)?;
        if n_in + n_out == 0 {
            return Err("OMT_INPUTS and OMT_OUTPUTS are both 0".into());
        }
        let label = var("OMT_LABEL_PREFIX").unwrap_or_else(|| "OMT".into());
        let inputs = (1..=n_in)
            .map(|i| InputConfig {
                source: var(&format!("OMT_IN{i}_SOURCE")).unwrap_or_default(),
            })
            .collect();
        let outputs = (1..=n_out)
            .map(|i| OutputConfig {
                name: var(&format!("OMT_OUT{i}_NAME")).unwrap_or_else(|| format!("{label} Out {i}")),
            })
            .collect::<Vec<_>>();
        for (i, a) in outputs.iter().enumerate() {
            if outputs[..i].iter().any(|b| b.name == a.name) {
                return Err(format!("OMT_OUT{}_NAME {:?} is used twice", i + 1, a.name));
            }
        }
        let in_loss = match var("OMT_IN_LOSS").as_deref() {
            None | Some("hold") => LossMode::Hold,
            Some("black") => LossMode::Black,
            Some(v) => return Err(format!("OMT_IN_LOSS={v:?}: expected hold or black")),
        };
        let out_pixel = match var("OMT_OUT_PIXEL").map(|v| v.to_ascii_lowercase()).as_deref() {
            None | Some("uyvy") => OutPixel::Uyvy,
            Some("p216") => OutPixel::P216,
            Some(v) => return Err(format!("OMT_OUT_PIXEL={v:?}: expected uyvy or p216")),
        };
        let port = |n: &str| -> Result<Option<i32>, String> {
            var(n).map(|v| v.parse::<i32>().map_err(|_| format!("{n}={v:?} is not a port"))).transpose()
        };
        Ok(Config {
            format,
            inputs,
            outputs,
            nmos_label: var("NMOS_LABEL").unwrap_or_else(|| "OMT bridge".into()),
            label,
            nmos_port: parse("NMOS_PORT", 3334)?,
            web_port: parse("WEB_PORT", 8200)?,
            output_domain_dir,
            scan_path,
            output_domain_id,
            seed,
            cleanup_on_exit: boolean("MXL_CLEANUP_ON_EXIT", false)?,
            history_ns: parse::<u64>("MXL_HISTORY_MS", 200)?.clamp(40, 10_000) * 1_000_000,
            state_dir: PathBuf::from(var("OMT_STATE_DIR").unwrap_or_else(|| "/config".into())),
            shutdown_timeout: Duration::from_secs(parse("SHUTDOWN_TIMEOUT_S", 10)?),
            audio_channels: ranged("AUDIO_CHANNELS", 2, 1, 32)?,
            in_loss,
            in_hold: Duration::from_millis(parse("OMT_IN_HOLD_MS", 1000)?),
            in_delay_ns: parse::<u64>("OMT_IN_DELAY_MS", 0)? * 1_000_000,
            in_scale: boolean("OMT_IN_SCALE", true)?,
            in_quality: quality("OMT_IN_QUALITY", "default")?,
            out_pixel,
            out_quality: quality("OMT_OUT_QUALITY", "default")?,
            out_read_offset: parse("OMT_OUT_READ_OFFSET_GRAINS", 1)?,
            out_hold_max: Duration::from_millis(parse("OMT_OUT_HOLD_MS", 1000)?),
            out_idle_black: boolean("OMT_OUT_IDLE_BLACK", true)?,
            omt_discovery_server: var("OMT_DISCOVERY_SERVER"),
            omt_port_start: port("OMT_PORT_START")?,
            omt_port_end: port("OMT_PORT_END")?,
            omt_log_file: var("OMT_LOG_FILE"),
            log_level: var("LOG_LEVEL").unwrap_or_else(|| "info".into()),
        })
    }

    /// The NMOS node's settings (`NMOS_*`).
    pub fn nmos_settings(&self) -> Result<mxl_nmos::Settings, String> {
        let mut s = mxl_nmos::Settings::new(self.nmos_label.clone(), self.nmos_port);
        s.description = "MXL ⇄ Open Media Transport bridge (lm-mxl-omt)".into();
        s.service_name_prefix = "lm-mxl-omt".into();
        s.apply_env().map_err(|e| e.to_string())?;
        if s.seed.is_none() {
            s.seed = Some(self.seed.clone());
        }
        Ok(s)
    }
}
