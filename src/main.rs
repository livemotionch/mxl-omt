//! lm-mxl-omt: start-up and shutdown. Exit codes as the mxl-stack functions:
//! 0, 75 (a port can't be bound, libmxl or libomt can't be loaded),
//! 78 (invalid configuration), 143 (SIGTERM).

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lm_mxl_omt::config::Config;
use lm_mxl_omt::input::Input;
use lm_mxl_omt::mxlio::{self, domain};
use lm_mxl_omt::output::Output;
use lm_mxl_omt::state::Shared;
use lm_mxl_omt::{api, nmos, omt};

const EX_CONFIG: u8 = 78;
const EX_UNAVAILABLE: u8 = 75;
const EX_TERM: u8 = 143;

fn fail(code: u8, message: impl std::fmt::Display) -> ExitCode {
    tracing::error!("{message}");
    eprintln!("lm-mxl-omt: {message}");
    ExitCode::from(code)
}

fn main() -> ExitCode {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("lm-mxl-omt: invalid configuration: {e}");
            return ExitCode::from(EX_CONFIG);
        }
    };
    tracing_subscriber::fmt()
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::new(format!("{},hyper=warn", config.log_level)))
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(r) => r,
        Err(e) => return fail(EX_UNAVAILABLE, format!("tokio runtime: {e}")),
    };
    runtime.block_on(run(config))
}

async fn run(config: Config) -> ExitCode {
    tracing::info!(
        "lm-mxl-omt {} starting: {}, {} OMT input(s), {} OMT output(s)",
        api::VERSION,
        config.format.name(),
        config.inputs.len(),
        config.outputs.len()
    );
    let tai_offset_s = mxl_nmos::clock::tai_utc_offset_secs();
    if tai_offset_s != mxl_nmos::clock::TAI_UTC_OFFSET_SECS as i64 {
        tracing::error!(
            "timing untrusted: CLOCK_TAI − CLOCK_REALTIME is {tai_offset_s} s, not {} s",
            mxl_nmos::clock::TAI_UTC_OFFSET_SECS
        );
    }
    if let Err(e) = domain::check_scan_path(&config.scan_path) {
        return fail(EX_CONFIG, e);
    }
    if let Err(e) = std::fs::create_dir_all(&config.state_dir) {
        return fail(EX_CONFIG, format!("OMT_STATE_DIR {}: {e}", config.state_dir.display()));
    }
    let api = match mxlio::load() {
        Ok(a) => a,
        Err(e) => return fail(EX_UNAVAILABLE, e),
    };
    // libomt: settings before the first sender or receiver.
    let omt = match omt::load() {
        Ok(o) => o,
        Err(e) => return fail(EX_UNAVAILABLE, e),
    };
    omt.set_logging_file(config.omt_log_file.as_deref());
    if let Some(s) = &config.omt_discovery_server {
        omt.set_string("DiscoveryServer", s);
    }
    if let Some(p) = config.omt_port_start {
        omt.set_integer("NetworkPortStart", p);
    }
    if let Some(p) = config.omt_port_end {
        omt.set_integer("NetworkPortEnd", p);
    }
    tracing::info!(
        "libomt: ports {}–{}, discovery {}",
        omt.get_integer("NetworkPortStart"),
        omt.get_integer("NetworkPortEnd"),
        match omt.get_string("DiscoveryServer") {
            s if s.is_empty() => "DNS-SD (Avahi)".to_string(),
            s => s,
        }
    );
    for (name, port) in [("WEB_PORT", config.web_port), ("NMOS_PORT", config.nmos_port)] {
        if let Err(e) = std::net::TcpListener::bind(("0.0.0.0", port)) {
            return fail(EX_UNAVAILABLE, format!("cannot bind {name} {port}: {e}"));
        }
    }
    let existed = config.output_domain_dir.exists();
    let mut config = config;
    let (created, foreign) = match domain::ensure_output_domain(
        &config.output_domain_dir,
        config.output_domain_id,
        &config.nmos_label,
        config.history_ns,
    ) {
        Ok(d) => {
            tracing::info!(
                "{} output domain {} ({})",
                if d.created { "created" } else { "using" },
                config.output_domain_dir.display(),
                d.id
            );
            config.output_domain_id = d.id;
            (d.created, d.foreign)
        }
        Err(e) => {
            if !existed || e.created {
                let _ = domain::remove_output_domain(&config.output_domain_dir, &config.scan_path);
            }
            return fail(EX_CONFIG, format!("MXL output domain: {e}"));
        }
    };
    let cleanup = config.cleanup_on_exit && !foreign;
    let abort = |config: &Config, code: u8, msg: String| {
        if created {
            let _ = domain::remove_output_domain(&config.output_domain_dir, &config.scan_path);
        }
        fail(code, msg)
    };

    // Input sources: the environment, else what the API set last time.
    let sources_path = config.state_dir.join("sources.json");
    let saved: Vec<String> = std::fs::read(&sources_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let sources: Vec<String> = config
        .inputs
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if !c.source.is_empty() {
                c.source.clone()
            } else {
                saved.get(i).cloned().unwrap_or_default()
            }
        })
        .collect();
    let shared = Shared::new(sources, config.outputs.len());
    let config = Arc::new(config);

    let mut inputs = Vec::new();
    for i in 0..config.inputs.len() {
        match Input::new(&api, &omt, &config.output_domain_dir, config.clone(), shared.clone(), i) {
            Ok(x) => inputs.push(x),
            Err(e) => return abort(&config, EX_CONFIG, format!("input {}: {e}", i + 1)),
        }
    }
    let mut outputs = Vec::new();
    for i in 0..config.outputs.len() {
        match Output::new(&api, &omt, config.clone(), shared.clone(), i) {
            Ok(x) => outputs.push(x),
            Err(e) => return abort(&config, EX_UNAVAILABLE, format!("output {}: {e}", i + 1)),
        }
    }

    let connections_path = config.state_dir.join("connections.json");
    let node = match nmos::build(&config, shared.clone(), connections_path.clone()) {
        Ok(n) => n,
        Err(e) => return abort(&config, EX_CONFIG, format!("NMOS: {e}")),
    };
    let saved = nmos::load_connections(&connections_path);
    if !saved.is_empty() {
        for (_, key, reason) in node.restore(saved).await {
            tracing::warn!("saved connection of {key} not restored: {reason}");
        }
    }
    let nmos_server = match node.bind(([0, 0, 0, 0], config.nmos_port).into()).await {
        Ok(s) => s,
        Err(e) => return abort(&config, EX_UNAVAILABLE, e.to_string()),
    };
    let web_addr: SocketAddr = ([0, 0, 0, 0], config.web_port).into();
    let web_listener = match tokio::net::TcpListener::bind(web_addr).await {
        Ok(l) => l,
        Err(e) => return abort(&config, EX_UNAVAILABLE, format!("cannot bind WEB_PORT {web_addr}: {e}")),
    };

    let mut threads = Vec::new();
    for (i, x) in inputs.into_iter().enumerate() {
        threads.push(
            std::thread::Builder::new()
                .name(format!("in-{}", i + 1))
                .spawn(move || x.run())
                .expect("input thread"),
        );
    }
    for (i, x) in outputs.into_iter().enumerate() {
        threads.push(
            std::thread::Builder::new()
                .name(format!("out-{}", i + 1))
                .spawn(move || x.run())
                .expect("output thread"),
        );
    }
    api::spawn_cpu_sampler(shared.clone());

    let app = api::App {
        config: config.clone(),
        shared: shared.clone(),
        omt: omt.clone(),
        node: node.clone(),
        started: Instant::now(),
        sources_path,
    };
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let web = tokio::spawn(async move {
        axum::serve(web_listener, api::router(app))
            .with_graceful_shutdown(async move {
                let _ = stop_rx.wait_for(|s| *s).await;
            })
            .await
    });
    let nmos_task = tokio::spawn(nmos_server.run());
    node.start_registration();
    tracing::info!("web on {web_addr}, NMOS node {} on {}:{}", node.node_id(), node.host(), config.nmos_port);

    let term = wait_for_signal().await;
    tracing::info!("shutting down");
    let deadline = config.shutdown_timeout;
    let sequence = async {
        shared.stop.store(true, Ordering::Relaxed);
        let _ = tokio::task::spawn_blocking(move || {
            for t in threads {
                let _ = t.join();
            }
        })
        .await;
        node.shutdown(deadline.saturating_sub(Duration::from_secs(1)).max(Duration::from_secs(1)))
            .await;
        if cleanup {
            match domain::remove_output_domain(&config.output_domain_dir, &config.scan_path) {
                Ok(()) => tracing::info!("removed output domain {}", config.output_domain_dir.display()),
                Err(e) => tracing::error!("output domain cleanup failed: {e}"),
            }
        }
        let _ = stop_tx.send(true);
        let _ = web.await;
        nmos_task.abort();
    };
    if tokio::time::timeout(deadline, sequence).await.is_err() {
        tracing::error!("shutdown did not finish within SHUTDOWN_TIMEOUT_S ({} s)", deadline.as_secs());
    }
    if term {
        ExitCode::from(EX_TERM)
    } else {
        ExitCode::SUCCESS
    }
}

/// `true` for SIGTERM, `false` for Ctrl-C.
async fn wait_for_signal() -> bool {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return false;
        }
    };
    tokio::select! {
        _ = term.recv() => true,
        _ = tokio::signal::ctrl_c() => false,
    }
}
