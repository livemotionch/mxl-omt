//! Web UI, JSON API, health and Prometheus metrics on `WEB_PORT`.
//!
//! - `GET /` the status page; `GET /api/status` everything as JSON
//! - `GET /api/sources` OMT sources discovery knows
//! - `PUT /api/inputs/{n}` `{"source": "HOST (Name)" | "omt://host:port" | ""}`
//! - `GET /livez`, `/readyz`, `/metrics`
//!
//! Unauthenticated, like the other mxl-stack functions: keep it on a
//! protected network (the test VM puts Caddy with basic auth in front).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde_json::json;

use crate::config::Config;
use crate::omt::OmtApi;
use crate::state::{lock, Shared};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const UI: &str = include_str!(env!("OMT_UI_HTML"));

#[derive(Clone)]
pub struct App {
    pub config: Arc<Config>,
    pub shared: Arc<Shared>,
    pub omt: OmtApi,
    pub node: mxl_nmos::Node,
    pub started: Instant,
    pub sources_path: PathBuf,
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/", get(|| async { Html(UI) }))
        .route("/api/status", get(status))
        .route("/api/sources", get(sources))
        .route("/api/inputs/{n}", put(set_input))
        .route("/livez", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
        .with_state(app)
}

async fn status(State(app): State<App>) -> Json<serde_json::Value> {
    let inputs: Vec<_> = app.shared.inputs.iter().map(|s| lock(&s.status).clone()).collect();
    let outputs: Vec<_> = app.shared.outputs.iter().map(|s| lock(&s.status).clone()).collect();
    let (cpu, threads) = lock(&app.shared.cpu).clone();
    let c = &app.config;
    Json(json!({
        "version": VERSION,
        "uptime_s": app.started.elapsed().as_secs(),
        "format": c.format.name(),
        "label": c.nmos_label,
        "nmos": {"node_id": app.node.node_id(), "registered": app.node.is_registered(), "port": c.nmos_port},
        "output_domain": {"id": c.output_domain_id, "path": c.output_domain_dir},
        "settings": {
            "in_loss": c.in_loss, "in_hold_ms": c.in_hold.as_millis() as u64,
            "in_delay_ms": c.in_delay_ns / 1_000_000, "in_scale": c.in_scale,
            "out_pixel": c.out_pixel, "out_quality": c.out_quality,
            "out_read_offset_grains": c.out_read_offset, "audio_channels": c.audio_channels,
            "discovery_server": c.omt_discovery_server,
        },
        "cpu_percent": cpu,
        "threads": threads,
        "inputs": inputs,
        "outputs": outputs,
    }))
}

async fn sources(State(app): State<App>) -> Json<Vec<String>> {
    let omt = app.omt.clone();
    Json(tokio::task::spawn_blocking(move || omt.discover()).await.unwrap_or_default())
}

#[derive(serde::Deserialize)]
struct SetInput {
    source: String,
}

async fn set_input(State(app): State<App>, Path(n): Path<usize>, Json(body): Json<SetInput>) -> Response {
    let Some(slot) = n.checked_sub(1).and_then(|i| app.shared.inputs.get(i)) else {
        return (StatusCode::NOT_FOUND, "no such input").into_response();
    };
    let source = body.source.trim().to_string();
    *lock(&slot.source) = source.clone();
    let all: Vec<String> = app.shared.inputs.iter().map(|s| lock(&s.source).clone()).collect();
    if let Err(e) = std::fs::write(&app.sources_path, serde_json::to_vec_pretty(&all).unwrap_or_default()) {
        tracing::error!("saving {}: {e}", app.sources_path.display());
    }
    Json(json!({"input": n, "source": source})).into_response()
}

async fn metrics(State(app): State<App>) -> Response {
    use std::fmt::Write;
    let mut m = String::new();
    let (cpu, _) = lock(&app.shared.cpu).clone();
    let _ = writeln!(m, "# TYPE lm_mxl_omt_cpu_percent gauge\nlm_mxl_omt_cpu_percent {cpu:.1}");
    for s in &app.shared.inputs {
        let st = lock(&s.status).clone();
        let l = format!("input=\"{}\"", st.index);
        for (k, v) in [
            ("frames_received_total", st.frames_received as f64),
            ("grains_written_total", st.grains_written as f64),
            ("dropped_total", st.dropped as f64),
            ("repeated_total", st.repeated as f64),
            ("black_total", st.black as f64),
            ("resyncs_total", st.resyncs as f64),
            ("audio_resyncs_total", st.audio_resyncs as f64),
            ("fps_in", st.fps_in),
            ("video_mbps", st.video_mbps),
            ("latency_ms", st.latency_ms.unwrap_or(f64::NAN)),
            ("transit_ms", st.transit_ms.unwrap_or(f64::NAN)),
        ] {
            let _ = writeln!(m, "lm_mxl_omt_in_{k}{{{l}}} {v}");
        }
    }
    for s in &app.shared.outputs {
        let st = lock(&s.status).clone();
        let l = format!("output=\"{}\"", st.index);
        for (k, v) in [
            ("frames_sent_total", st.frames_sent as f64),
            ("late_total", st.late as f64),
            ("held_total", st.held as f64),
            ("black_total", st.black as f64),
            ("skipped_total", st.skipped as f64),
            ("audio_gaps_total", st.audio_gaps as f64),
            ("connections", st.connections as f64),
            ("fps", st.fps),
            ("video_mbps", st.video_mbps),
        ] {
            let _ = writeln!(m, "lm_mxl_omt_out_{k}{{{l}}} {v}");
        }
    }
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], m).into_response()
}

/// Sample CPU once a second: the process, and each thread by name (`in-1`,
/// `out-1`, and libomt's own threads), in % of one core.
pub fn spawn_cpu_sampler(shared: Arc<Shared>) {
    std::thread::Builder::new()
        .name("cpu".into())
        .spawn(move || {
            let tick = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
            let read = |p: &std::path::Path| -> Option<(String, u64)> {
                let s = std::fs::read_to_string(p.join("stat")).ok()?;
                let (l, r) = (s.find('(')?, s.rfind(')')?);
                let name = s[l + 1..r].to_string();
                let f: Vec<&str> = s[r + 2..].split(' ').collect();
                Some((name, f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?))
            };
            let mut prev: std::collections::HashMap<String, u64> = Default::default();
            let mut prev_total = None;
            let mut last = Instant::now();
            while !shared.stopping() {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let dt = last.elapsed().as_secs_f64();
                last = Instant::now();
                let total = read(std::path::Path::new("/proc/self")).map(|(_, t)| t);
                let mut now: std::collections::HashMap<String, u64> = Default::default();
                let mut by_name: std::collections::BTreeMap<String, f64> = Default::default();
                if let Ok(dir) = std::fs::read_dir("/proc/self/task") {
                    for e in dir.flatten() {
                        if let Some((name, t)) = read(&e.path()) {
                            let tid = e.file_name().to_string_lossy().into_owned();
                            if let Some(p) = prev.get(&tid) {
                                *by_name.entry(name).or_default() += (t.saturating_sub(*p)) as f64 / tick / dt * 100.0;
                            }
                            now.insert(tid, t);
                        }
                    }
                }
                prev = now;
                let cpu = match (total, prev_total) {
                    (Some(t), Some(p)) => (t - p) as f64 / tick / dt * 100.0,
                    _ => 0.0,
                };
                prev_total = total;
                let mut threads: Vec<(String, f64)> = by_name.into_iter().filter(|(_, v)| *v >= 0.5).collect();
                threads.sort_by(|a, b| b.1.total_cmp(&a.1));
                *lock(&shared.cpu) = (cpu, threads);
            }
        })
        .expect("cpu thread");
}
