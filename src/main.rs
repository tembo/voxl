//! voxl: low-latency X11 desktop streaming (H.264 over WebSocket) for the browser.

mod audio;
mod capture;
mod convert;
mod desktop;
mod encoder;
mod hub;
mod input;
mod server;
mod video;

use std::collections::HashMap;
use std::time::Instant;

/// Parses `--key=value` / `--key value` / `--flag`; unknown options are accepted so the
/// binary can take the same argument list as the Python selkies service it replaces.
fn parse_args() -> HashMap<String, String> {
    let mut m = HashMap::new();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(k) = a.strip_prefix("--") {
            if let Some((k, v)) = k.split_once('=') {
                m.insert(k.replace('_', "-"), v.to_string());
            } else if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                m.insert(k.replace('_', "-"), args[i + 1].clone());
                i += 1;
            } else {
                m.insert(k.replace('_', "-"), "true".into());
            }
        }
        i += 1;
    }
    m
}

/// Selkies ranges look like "60-60"; take the upper bound.
fn range_max(v: Option<&String>, default: f64) -> f64 {
    v.and_then(|s| s.rsplit('-').next()?.trim().parse().ok()).unwrap_or(default)
}

fn truthy(v: Option<&String>, default: bool) -> bool {
    v.map(|s| matches!(s.as_str(), "true" | "1" | "yes" | "on")).unwrap_or(default)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let a = parse_args();
    let display = std::env::var("DISPLAY").ok().or_else(|| a.get("display").cloned());
    let addr = a.get("addr").cloned().unwrap_or_else(|| "0.0.0.0".into());
    let port: u16 = a.get("port").and_then(|p| p.parse().ok()).unwrap_or(6901);
    let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) as u32;
    let vcfg = video::VideoConfig {
        display: display.clone(),
        fps: range_max(a.get("framerate"), 60.0) as u32,
        preset: a.get("preset").cloned().unwrap_or_else(|| "ultrafast".into()),
        // Four sliced threads: lower latency than one, and x264's slicing overhead grows
        // with thread count, so more threads only add CPU (measured on 2560x1600).
        threads: a.get("encoder-threads").and_then(|v| v.parse().ok()).unwrap_or((ncpu / 4).clamp(1, 4)),
        convert_threads: a.get("convert-threads").and_then(|v| v.parse().ok()).unwrap_or(4),
        crf: range_max(a.get("crf"), 24.0) as f32,
        // Selkies' --video-bitrate becomes the ceiling for adaptive bitrate.
        max_kbps: range_max(a.get("max-kbps").or(a.get("video-bitrate")), 30000.0) as u32,
        paintover_crf: range_max(a.get("paintover-crf"), 16.0) as f32,
        paintover_burst: range_max(a.get("paintover-burst"), 3.0) as u32,
        paintover_delay_ms: range_max(a.get("paintover-delay-ms"), 150.0) as u64,
        min_kbps: range_max(a.get("min-kbps"), 800.0) as u32,
        stripes: range_max(a.get("stripes"), 1.0) as u32,
        adaptive_threads: truthy(a.get("adaptive-threads"), false),
        coalesce: std::time::Duration::from_micros(range_max(a.get("coalesce-us"), 0.0) as u64),
    };
    let clip = a.get("enable-clipboard").map(|s| s.as_str()).unwrap_or("true");
    let clip_mode = desktop::ClipboardMode { to_server: matches!(clip, "true" | "in"), to_client: matches!(clip, "true" | "out") };
    let audio_on = truthy(a.get("audio-enabled"), true);

    let hub = hub::Hub::new();
    if let Err(e) = video::spawn(vcfg, hub.clone()) {
        log::error!("video init failed: {e}");
        std::process::exit(1);
    }
    let clipboard = match desktop::spawn(display.clone(), hub.clone(), clip_mode) {
        Ok(h) => Some(h),
        Err(e) => {
            log::error!("cursor/clipboard init failed: {e}");
            None
        }
    };
    match input::spawn(display.clone(), clipboard) {
        Ok(tx) => *hub.input.lock() = Some(tx),
        Err(e) => log::error!("input init failed: {e}"),
    }
    if audio_on {
        let dev = a.get("audio-device-name").cloned().unwrap_or_else(|| "output.monitor".into());
        let br = range_max(a.get("audio-bitrate"), 128000.0) as i32;
        audio::spawn(dev, br, hub.clone());
    }

    let state = server::AppState { hub: hub.clone(), audio: audio_on, epoch: Instant::now() };
    let app = server::router(state);
    let listener = tokio::net::TcpListener::bind((addr.as_str(), port)).await.expect("bind");
    log::info!("listening on http://{addr}:{port}");
    use axum::serve::ListenerExt;
    let listener = listener.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    let shutdown_hub = hub.clone();
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).unwrap();
        let mut int = signal(SignalKind::interrupt()).unwrap();
        tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
        log::info!("shutting down");
        if let Some(tx) = shutdown_hub.input.lock().as_ref() {
            let _ = tx.send(input::InputEvent::Shutdown);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        std::process::exit(0);
    });
    axum::serve(listener, app).await.unwrap();
}
