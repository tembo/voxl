//! HTTP + WebSocket front end. Serves the embedded web client and the media/input socket.

use crate::hub::{Client, Hub, Out};
use crate::input::InputEvent;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<Hub>,
    pub audio: bool,
    pub epoch: Instant,
}

macro_rules! asset {
    ($name:literal, $ctype:literal) => {
        get(|| async { ([(header::CONTENT_TYPE, $ctype), (header::CACHE_CONTROL, "no-cache")], include_bytes!(concat!("../web/", $name)).as_slice()) })
    };
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", asset!("index.html", "text/html; charset=utf-8"))
        .route("/index.html", asset!("index.html", "text/html; charset=utf-8"))
        .route("/client.js", asset!("client.js", "text/javascript"))
        .route("/worker.js", asset!("worker.js", "text/javascript"))
        .route("/audio.js", asset!("audio.js", "text/javascript"))
        .route("/favicon.ico", asset!("favicon.ico", "image/x-icon"))
        .route("/healthz", get(|| async { "ok" }))
        .route("/stats", get(stats))
        .route("/ws", get(ws_handler))
        .with_state(state)
}

async fn stats(State(s): State<AppState>) -> Response {
    let st = &s.hub.stats;
    let clients: Vec<_> = s
        .hub
        .clients
        .lock()
        .iter()
        .map(|c| {
            let (n, age) = c.backlog();
            serde_json::json!({"id": c.id, "video": c.video.load(Ordering::Relaxed), "inflight": n, "oldest_ms": age,
                "min_rtt_ms": c.min_rtt_ms.load(Ordering::Relaxed), "queue_ms": c.delay.lock().queue_ms as u64, "delivered_kbps": (c.delivered.lock().rate_bps / 1000.0) as u64})
        })
        .collect();
    let body = serde_json::json!({
        "captured": st.captured.load(Ordering::Relaxed),
        "encoded": st.encoded.load(Ordering::Relaxed),
        "bytes": st.bytes.load(Ordering::Relaxed),
        "encode_us_total": st.encode_us.load(Ordering::Relaxed),
        "grab_us_total": st.grab_us.load(Ordering::Relaxed),
        "convert_us_total": st.convert_us.load(Ordering::Relaxed),
        "flow_blocked": st.flow_blocked.load(Ordering::Relaxed),
        "paintovers": st.paintovers.load(Ordering::Relaxed),
        "keyframes": st.keyframes.load(Ordering::Relaxed),
        "refines": st.refines.load(Ordering::Relaxed),
        "target_kbps": s.hub.target_kbps.load(Ordering::Relaxed),
        "damage_events": crate::capture::DAMAGE_EVENTS.load(Ordering::Relaxed),
        "damage_wait_us_total": st.damage_wait_us.load(Ordering::Relaxed),
        "send_lat_us_total": st.send_lat_us.load(Ordering::Relaxed),
        "clients": clients,
    });
    ([(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

async fn ws_handler(ws: WebSocketUpgrade, State(s): State<AppState>) -> Response {
    ws.max_message_size(64 << 20).on_upgrade(move |socket| handle_socket(socket, s))
}

async fn handle_socket(socket: WebSocket, s: AppState) {
    use futures_util::{SinkExt, StreamExt};
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    let client = s.hub.add_client(tx);
    log::info!("client {} connected", client.id);
    let stripes = s.hub.stripes.lock().clone();
    let hello = serde_json::json!({"t": "hello", "w": s.hub.width.load(Ordering::Relaxed), "h": s.hub.height.load(Ordering::Relaxed), "audio": s.audio, "stripes": stripes});
    client.send(Out::Text(hello.to_string()));
    if let Some(c) = s.hub.cursor.lock().clone() {
        client.send(Out::Bin(c));
    }
    let writer = tokio::spawn(async move {
        let dbg = std::env::var("VOXL_DEBUG_MARKER").is_ok();
        while let Some(m) = rx.recv().await {
            let t = Instant::now();
            let big = matches!(&m, Out::Bin(b) if b.len() > 2000);
            let msg = match m {
                Out::Bin(b) => Message::Binary(b),
                Out::Text(t) => Message::Text(t.into()),
            };
            if sink.send(msg).await.is_err() {
                break;
            }
            if dbg && big {
                log::info!("ws send took {} us (queued {})", t.elapsed().as_micros(), rx.len());
            }
        }
    });
    let pinger = {
        let c = client.clone();
        let epoch = s.epoch;
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_millis(500));
            loop {
                iv.tick().await;
                let t = epoch.elapsed().as_micros() as u64;
                c.send(Out::Text(format!("{{\"t\":\"ping\",\"s\":{t}}}")));
            }
        })
    };
    let mut rtts: std::collections::VecDeque<u32> = Default::default();
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(t) => handle_text(&s, &client, t.as_str(), &mut rtts),
            Message::Close(_) => break,
            _ => {}
        }
    }
    pinger.abort();
    writer.abort();
    if let Some(tx) = s.hub.input.lock().as_ref() {
        let _ = tx.send(InputEvent::ReleaseAll);
    }
    s.hub.remove_client(client.id);
    log::info!("client {} disconnected", client.id);
}

fn num<T: std::str::FromStr>(it: &mut std::str::Split<'_, char>) -> Option<T> {
    it.next()?.trim().parse().ok()
}

fn handle_text(s: &AppState, c: &Arc<Client>, t: &str, rtts: &mut std::collections::VecDeque<u32>) {
    let (cmd, rest) = t.split_once(',').unwrap_or((t, ""));
    let input = |ev: InputEvent| {
        if let Some(tx) = s.hub.input.lock().as_ref() {
            let _ = tx.send(ev);
        }
    };
    let mut it = rest.split(',');
    match cmd {
        "a" => {
            if let Some(id) = num::<u32>(&mut it) {
                c.on_ack(id);
                s.hub.wake();
            }
        }
        "m" => {
            if let (Some(x), Some(y)) = (num(&mut it), num(&mut it)) {
                input(InputEvent::Move { x, y });
            }
        }
        "b" => {
            if let (Some(button), Some(d)) = (num::<u8>(&mut it), num::<u8>(&mut it)) {
                input(InputEvent::Button { button, down: d == 1 });
            }
        }
        "s" => {
            if let (Some(dx), Some(dy)) = (num(&mut it), num(&mut it)) {
                input(InputEvent::Scroll { dx, dy });
            }
        }
        "k" => {
            if let (Some(keysym), Some(d)) = (num::<u32>(&mut it), num::<u8>(&mut it)) {
                input(InputEvent::Key { keysym, down: d == 1 });
            }
        }
        "r" => input(InputEvent::ReleaseAll),
        "c" => input(InputEvent::Clipboard(rest.to_string())),
        "v" => {
            let on = rest == "1";
            c.video.store(on, Ordering::Relaxed);
            if on {
                c.inflight.lock().clear();
                c.started.store(false, Ordering::Relaxed);
                c.needs_keyframe.store(true, Ordering::Relaxed);
            }
            s.hub.wake();
        }
        "au" => c.audio.store(rest == "1" && s.audio, Ordering::Relaxed),
        "kf" => {
            c.started.store(false, Ordering::Relaxed);
            c.needs_keyframe.store(true, Ordering::Relaxed);
            s.hub.wake();
        }
        "po" => {
            if let Some(sent) = num::<u64>(&mut it) {
                let now = s.epoch.elapsed().as_micros() as u64;
                let rtt = (now.saturating_sub(sent) / 1000) as u32;
                rtts.push_back(rtt.max(1));
                if rtts.len() > 20 {
                    rtts.pop_front();
                }
                c.min_rtt_ms.store(*rtts.iter().min().unwrap(), Ordering::Relaxed);
            }
        }
        _ => {}
    }
}
