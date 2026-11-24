//! MXL flow definitions (NMOS IS-04 flow JSON): the mixer's outputs, and what
//! it needs to know about an input flow.

use serde_json::{json, Value};
use uuid::Uuid;

/// The video format of the house (all outputs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub rate_n: u32,
    pub rate_d: u32,
}

impl VideoFormat {
    /// `1080p50`, `720p59.94`, `2160p25`, `1080p29.97`, ...
    pub fn parse(s: &str) -> Result<VideoFormat, String> {
        let s = s.trim().to_ascii_lowercase();
        let (lines, rate) = s
            .split_once('p')
            .ok_or_else(|| format!("video format {s:?}: expected e.g. 1080p50 (progressive only)"))?;
        let (width, height) = match lines {
            "720" => (1280, 720),
            "1080" => (1920, 1080),
            "2160" => (3840, 2160),
            other => return Err(format!("video format {s:?}: {other} lines not supported (720, 1080, 2160)")),
        };
        let (rate_n, rate_d) = match rate {
            "23.98" | "23.976" => (24000, 1001),
            "24" => (24, 1),
            "25" => (25, 1),
            "29.97" => (30000, 1001),
            "30" => (30, 1),
            "50" => (50, 1),
            "59.94" => (60000, 1001),
            "60" => (60, 1),
            other => return Err(format!("video format {s:?}: rate {other} not supported")),
        };
        Ok(VideoFormat {
            width,
            height,
            rate_n,
            rate_d,
        })
    }

    pub fn frame_ns(&self) -> u64 {
        1_000_000_000u64 * self.rate_d as u64 / self.rate_n as u64
    }

    pub fn fps(&self) -> f64 {
        self.rate_n as f64 / self.rate_d as f64
    }

    pub fn name(&self) -> String {
        let rate = match (self.rate_n, self.rate_d) {
            (n, 1) => n.to_string(),
            (n, d) => format!("{:.2}", n as f64 / d as f64),
        };
        format!("{}p{}", self.height, rate)
    }
}

/// The grouphint tag value of a flow of this instance.
pub fn grouphint(group: &str, role: &str) -> Value {
    json!({ "urn:x-nmos:tag:grouphint/v1.0": [format!("{group}:{role}")] })
}

/// A v210 (or v210a with `key`) flow definition.
pub fn video(id: Uuid, label: &str, description: &str, group: &str, f: &VideoFormat, key: bool) -> String {
    let mut components = vec![
        json!({"name": "Y", "width": f.width, "height": f.height, "bit_depth": 10}),
        json!({"name": "Cb", "width": f.width / 2, "height": f.height, "bit_depth": 10}),
        json!({"name": "Cr", "width": f.width / 2, "height": f.height, "bit_depth": 10}),
    ];
    if key {
        components.push(json!({"name": "A", "width": f.width, "height": f.height, "bit_depth": 10}));
    }
    json!({
        "id": id.to_string(),
        "description": description,
        "tags": grouphint(group, "Video"),
        "format": "urn:x-nmos:format:video",
        "label": label,
        "parents": [],
        "media_type": if key { "video/v210a" } else { "video/v210" },
        "grain_rate": {"numerator": f.rate_n, "denominator": f.rate_d},
        "frame_width": f.width,
        "frame_height": f.height,
        "interlace_mode": "progressive",
        "colorspace": "BT709",
        "components": components,
    })
    .to_string()
}

/// A float32 48 kHz audio flow definition.
pub fn audio(id: Uuid, label: &str, description: &str, group: &str, channels: usize) -> String {
    json!({
        "id": id.to_string(),
        "description": description,
        "tags": grouphint(group, "Audio"),
        "format": "urn:x-nmos:format:audio",
        "label": label,
        "parents": [],
        "media_type": "audio/float32",
        "sample_rate": {"numerator": 48000, "denominator": 1},
        "channel_count": channels,
        "bit_depth": 32,
    })
    .to_string()
}

/// What the mixer reads from an input flow's definition.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct InputFlow {
    pub label: String,
    pub media_type: String,
    pub width: usize,
    pub height: usize,
    pub rate_n: u32,
    pub rate_d: u32,
    pub interlaced: bool,
}

impl InputFlow {
    pub fn parse(def: &str) -> Result<InputFlow, String> {
        let v: Value = serde_json::from_str(def).map_err(|e| format!("flow definition: {e}"))?;
        let media_type = v["media_type"].as_str().unwrap_or_default().to_string();
        if media_type != "video/v210" && media_type != "video/v210a" {
            return Err(format!("media type {media_type:?} is not video/v210 or video/v210a"));
        }
        let num = |k: &str| v[k].as_f64().unwrap_or(0.0) as usize;
        let interlaced = v["interlace_mode"].as_str().is_some_and(|m| m != "progressive");
        Ok(InputFlow {
            label: v["label"].as_str().unwrap_or_default().to_string(),
            media_type,
            width: num("frame_width"),
            height: num("frame_height"),
            rate_n: v["grain_rate"]["numerator"].as_u64().unwrap_or(0) as u32,
            rate_d: v["grain_rate"]["denominator"].as_u64().unwrap_or(1).max(1) as u32,
            interlaced,
        })
    }

    pub fn has_key(&self) -> bool {
        self.media_type == "video/v210a"
    }

    /// Why the mixer can't read it, if it can't.
    pub fn unsupported(&self) -> Option<String> {
        if self.interlaced {
            return Some("interlaced flows are not supported".into());
        }
        if self.width == 0 || self.height == 0 || self.width % 2 != 0 {
            return Some(format!("frame size {}x{} not supported", self.width, self.height));
        }
        if self.rate_n == 0 {
            return Some("no grain rate".into());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        let f = VideoFormat::parse("1080p50").unwrap();
        assert_eq!((f.width, f.height, f.rate_n, f.rate_d), (1920, 1080, 50, 1));
        assert_eq!(f.frame_ns(), 20_000_000);
        assert_eq!(f.name(), "1080p50");
        let f = VideoFormat::parse("720p59.94").unwrap();
        assert_eq!((f.rate_n, f.rate_d), (60000, 1001));
        assert_eq!(f.name(), "720p59.94");
        assert!(VideoFormat::parse("1080i50").is_err());
        assert!(VideoFormat::parse("576p25").is_err());
    }

    #[test]
    fn video_defs_parse_back() {
        let f = VideoFormat::parse("1080p50").unwrap();
        let id = Uuid::new_v4();
        let def = video(id, "VM PGM", "PGM of the vision mixer", "VM", &f, false);
        let input = InputFlow::parse(&def).unwrap();
        assert_eq!(input.width, 1920);
        assert_eq!(input.rate_n, 50);
        assert!(!input.has_key() && input.unsupported().is_none());
        let def = video(id, "VM Key", "", "VM", &f, true);
        assert!(InputFlow::parse(&def).unwrap().has_key());
        let v: Value = serde_json::from_str(&audio(id, "a", "", "VM", 2)).unwrap();
        assert_eq!(v["channel_count"], 2);
        assert!(InputFlow::parse(&audio(id, "a", "", "VM", 2)).is_err());
    }
}
