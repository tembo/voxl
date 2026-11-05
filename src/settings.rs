//! Settings viewers change from the toolbar's cog while voxl runs. Flags set where each
//! one starts, and a change lasts until voxl restarts. What a flag allows (clipboard
//! directions, file transfers, the upload limit, the longest recording) can be narrowed
//! here but never widened, so a viewer can't undo what whoever started voxl locked down.
//!
//! The page sends `set,<key>,<value>`; every viewer then gets the new `settings` message.

use crate::files::Files;
use crate::hub::{Hub, Out};
use crate::record::Recorder;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// x264's presets, fastest first.
const PRESETS: &[&str] = &[
    "ultrafast",
    "superfast",
    "veryfast",
    "faster",
    "fast",
    "medium",
    "slow",
    "slower",
    "veryslow",
];

/// Settings that rebuild the encoder: the stream pauses briefly and restarts with a
/// keyframe. Everything else applies to the next frame.
const RESTART: &[&str] = &[
    "framerate",
    "auto_framerate",
    "preset",
    "encoder_threads",
    "adaptive_threads",
    "stripes",
];

const MB: f64 = 1048576.0;

pub struct Settings {
    hub: Arc<Hub>,
    files: Arc<Files>,
    recorder: Arc<Recorder>,
    /// What --enable-clipboard allows, viewer to desktop and back.
    clip_in_allowed: bool,
    clip_out_allowed: bool,
    clip_in: AtomicBool,
    clip_out: AtomicBool,
    /// Desktop audio is streamed (--audio-enabled).
    audio: bool,
    /// The desktop follows the viewers' windows (with --enable-resize); off, it keeps
    /// the resolution picked in the panel.
    auto_resize: AtomicBool,
    /// The video settings voxl started with, which the Auto choices go back to.
    initial: Option<crate::video::VideoConfig>,
    /// Smoothness on Auto: --framerate's rate, with flow control dropping frames when
    /// the link can't keep up. Off: the panel picked a rate.
    auto_framerate: AtomicBool,
    /// --audio-bitrate, which Auto sound goes back to.
    initial_audio: i32,
    /// The desktop's size when voxl started: the host's own, which the panel
    /// recommends.
    native: (u32, u32),
    /// --resize-scale, which the panel's Scaling percentages are relative to.
    initial_scale: Option<f64>,
}

impl Settings {
    pub fn new(
        hub: Arc<Hub>,
        files: Arc<Files>,
        recorder: Arc<Recorder>,
        clip_in: bool,
        clip_out: bool,
        audio: bool,
    ) -> Arc<Self> {
        let initial = hub.video_cfg.lock().clone();
        let initial_audio = hub.audio_auto.load(Ordering::Relaxed);
        let native = (
            hub.width.load(Ordering::Relaxed),
            hub.height.load(Ordering::Relaxed),
        );
        let initial_scale = hub
            .hello_extra
            .lock()
            .get("resize")
            .and_then(|r| r.get("scale"))
            .and_then(|s| s.as_f64());
        Arc::new(Settings {
            hub,
            files,
            recorder,
            clip_in_allowed: clip_in,
            clip_out_allowed: clip_out,
            clip_in: AtomicBool::new(clip_in),
            clip_out: AtomicBool::new(clip_out),
            audio,
            auto_resize: AtomicBool::new(true),
            initial_audio,
            native,
            initial_scale,
            initial,
            auto_framerate: AtomicBool::new(true),
        })
    }

    /// Whether the desktop takes clipboard contents from viewers right now.
    pub fn clip_in(&self) -> bool {
        self.clip_in.load(Ordering::Relaxed)
    }

    /// Whether viewers get the desktop's clipboard right now.
    pub fn clip_out(&self) -> bool {
        self.clip_out.load(Ordering::Relaxed)
    }

    /// Whether a viewer's window size resizes the desktop right now.
    pub fn auto_resize(&self) -> bool {
        self.auto_resize.load(Ordering::Relaxed)
    }

    /// Desktop pixels per CSS pixel, when dynamic resolution is on.
    fn resize_scale(&self) -> Option<f64> {
        self.hub
            .hello_extra
            .lock()
            .get("resize")?
            .get("scale")?
            .as_f64()
    }

    /// `{"t":"settings","values":{...},"limits":{...},"restart":[...]}`
    pub fn message(&self) -> String {
        let v = self.hub.video_cfg.lock().clone();
        let mut values = json!({
            "audio_bitrate": self.hub.audio_bitrate.load(Ordering::Relaxed),
            "auto_audio": self.hub.audio_auto.load(Ordering::Relaxed) > 0,
            "auto_framerate": self.auto_framerate.load(Ordering::Relaxed),
            "auto_framerate_fps": self.initial.as_ref().map(|i| i.fps),
            "clipboard_in": self.clip_in(),
            "clipboard_out": self.clip_out(),
            "upload": self.files.can_upload(),
            "download": self.files.can_download(),
            "max_upload_mb": (self.files.upload_limit() as f64 / MB).round(),
            "max_record_seconds": self.recorder.max_len().as_secs(),
            "resize_scale": self.resize_scale(),
            "auto_resize": self.resize_scale().map(|_| self.auto_resize()),
            "resolution": format!(
                "{}x{}",
                self.hub.width.load(Ordering::Relaxed),
                self.hub.height.load(Ordering::Relaxed)
            ),
        });
        if let Some(c) = v {
            let m = values.as_object_mut().unwrap();
            for (k, x) in [
                ("framerate", json!(c.fps)),
                ("preset", json!(c.preset)),
                ("encoder_threads", json!(c.threads)),
                ("adaptive_threads", json!(c.adaptive_threads)),
                ("stripes", json!(c.stripes)),
                ("crf", json!(c.crf)),
                ("auto_quality", json!(c.auto_quality)),
                ("max_kbps", json!(c.max_kbps)),
                ("min_kbps", json!(c.min_kbps)),
                ("paintover_crf", json!(c.paintover_crf)),
                ("paintover_burst", json!(c.paintover_burst)),
                ("paintover_delay_ms", json!(c.paintover_delay_ms)),
                ("convert_threads", json!(c.convert_threads)),
                ("coalesce_us", json!(c.coalesce.as_micros() as u64)),
            ] {
                m.insert(k.into(), x);
            }
        }
        json!({
            "t": "settings",
            "values": values,
            "limits": {
                "audio": self.audio,
                "resize": self.resize_scale().is_some(),
                "clipboard_in": self.clip_in_allowed,
                "clipboard_out": self.clip_out_allowed,
                "upload": self.files.upload,
                "download": self.files.download,
                "max_upload_mb": (self.files.max_upload_bytes as f64 / MB).round(),
                "max_record_seconds": self.recorder.max.as_secs(),
                "native": format!("{}x{}", self.native.0, self.native.1),
                "scale_default": self.initial_scale,
                "resolutions": self.hub.resize_sizes.lock().iter()
                    .map(|(w, h)| format!("{w}x{h}")).collect::<Vec<_>>(),
            },
            "restart": RESTART,
        })
        .to_string()
    }

    /// Sends every viewer the current settings.
    pub fn broadcast(&self) {
        let m = self.message();
        self.hub.broadcast(|| Out::Text(m.clone()));
    }

    /// Changes one setting. The error says why a value was refused.
    pub fn set(&self, key: &str, v: &str) -> Result<(), String> {
        let num = |lo: f64, hi: f64| -> Result<f64, String> {
            let x: f64 = v
                .trim()
                .parse()
                .map_err(|_| format!("{key}: {v:?} is not a number"))?;
            if x.is_finite() && (lo..=hi).contains(&x) {
                Ok(x)
            } else {
                Err(format!("{key} must be between {lo} and {hi}"))
            }
        };
        let flag = || match v {
            "true" | "1" => Ok(true),
            "false" | "0" => Ok(false),
            _ => Err(format!("{key}: {v:?} is not true or false")),
        };
        let narrow = |on: bool, allowed: bool, flag_name: &str| {
            if on && !allowed {
                Err(format!("{key} is turned off by {flag_name}"))
            } else {
                Ok(on)
            }
        };
        match key {
            "framerate" | "preset" | "encoder_threads" | "adaptive_threads" | "stripes" | "crf"
            | "auto_quality" | "max_kbps" | "min_kbps" | "paintover_crf" | "paintover_burst"
            | "paintover_delay_ms" | "convert_threads" | "coalesce_us" => {
                let mut g = self.hub.video_cfg.lock();
                let c = g.as_mut().ok_or("video isn't running")?;
                match key {
                    "framerate" => {
                        c.fps = num(1.0, 120.0)? as u32;
                        self.auto_framerate.store(false, Ordering::Relaxed);
                    }
                    "preset" => {
                        if !PRESETS.contains(&v) {
                            return Err(format!("preset must be one of {}", PRESETS.join(", ")));
                        }
                        c.preset = v.to_string();
                    }
                    "encoder_threads" => c.threads = num(1.0, 64.0)? as u32,
                    "adaptive_threads" => c.adaptive_threads = flag()?,
                    "stripes" => c.stripes = num(1.0, 16.0)? as u32,
                    // A quality of one's own: Auto stops moving it.
                    "crf" => {
                        c.crf = num(0.0, 51.0)? as f32;
                        c.auto_quality = false;
                    }
                    // Auto quality starts from voxl's own settings again.
                    "auto_quality" => {
                        c.auto_quality = flag()?;
                        if let (true, Some(i)) = (c.auto_quality, &self.initial) {
                            c.crf = i.crf;
                            c.max_kbps = i.max_kbps;
                            c.min_kbps = i.min_kbps;
                            c.paintover_crf = i.paintover_crf;
                        }
                    }
                    "paintover_crf" => c.paintover_crf = num(0.0, 51.0)? as f32,
                    "max_kbps" => {
                        c.max_kbps = num(100.0, 500_000.0)? as u32;
                        c.min_kbps = c.min_kbps.min(c.max_kbps);
                    }
                    "min_kbps" => {
                        c.min_kbps = num(100.0, 500_000.0)? as u32;
                        c.max_kbps = c.max_kbps.max(c.min_kbps);
                    }
                    "paintover_burst" => c.paintover_burst = num(0.0, 10.0)? as u32,
                    "paintover_delay_ms" => c.paintover_delay_ms = num(0.0, 10_000.0)? as u64,
                    "convert_threads" => c.convert_threads = num(1.0, 64.0)? as usize,
                    "coalesce_us" => {
                        c.coalesce = std::time::Duration::from_micros(num(0.0, 100_000.0)? as u64)
                    }
                    _ => unreachable!(),
                }
                drop(g);
                self.hub.video_dirty.store(true, Ordering::Relaxed);
                self.hub.wake();
            }
            "audio_bitrate" if self.audio => {
                let b = num(6000.0, 510_000.0)? as i32;
                self.hub.audio_auto.store(0, Ordering::Relaxed);
                self.hub.audio_bitrate.store(b, Ordering::Relaxed);
            }
            // Auto sound: up to --audio-bitrate again, lower on slow links.
            "auto_audio" if self.audio => {
                let on = flag()?;
                let top = if on { self.initial_audio } else { 0 };
                self.hub.audio_auto.store(top, Ordering::Relaxed);
                if on {
                    self.hub.audio_bitrate.store(top, Ordering::Relaxed);
                }
            }
            // Auto smoothness: --framerate again.
            "auto_framerate" => {
                let on = flag()?;
                self.auto_framerate.store(on, Ordering::Relaxed);
                if let (true, Some(i)) = (on, &self.initial) {
                    let mut g = self.hub.video_cfg.lock();
                    let c = g.as_mut().ok_or("video isn't running")?;
                    c.fps = i.fps;
                    drop(g);
                    self.hub.video_dirty.store(true, Ordering::Relaxed);
                    self.hub.wake();
                }
            }
            "clipboard_in" => {
                let on = narrow(flag()?, self.clip_in_allowed, "--enable-clipboard")?;
                self.clip_in.store(on, Ordering::Relaxed);
            }
            "clipboard_out" => {
                let on = narrow(flag()?, self.clip_out_allowed, "--enable-clipboard")?;
                self.clip_out.store(on, Ordering::Relaxed);
            }
            "upload" => {
                let on = narrow(flag()?, self.files.upload, "--file-transfers")?;
                self.files.live.upload_off.store(!on, Ordering::Relaxed);
                self.update_files_hello();
            }
            "download" => {
                let on = narrow(flag()?, self.files.download, "--file-transfers")?;
                self.files.live.download_off.store(!on, Ordering::Relaxed);
                self.update_files_hello();
            }
            "max_upload_mb" => {
                // 0 is no limit, which only --max-upload-mb=0 allows.
                let ceiling = self.files.max_upload_bytes as f64 / MB;
                let mb = num(0.0, if ceiling > 0.0 { ceiling } else { 1e9 })?;
                if ceiling > 0.0 && mb == 0.0 {
                    return Err(format!(
                        "max_upload_mb can't be above --max-upload-mb ({ceiling})"
                    ));
                }
                let bytes = (mb * MB) as u64;
                self.files
                    .live
                    .max_upload_bytes
                    .store(bytes, Ordering::Relaxed);
            }
            "max_record_seconds" => {
                let s = num(1.0, self.recorder.max.as_secs() as f64)? as u64;
                self.recorder.limit_secs.store(s, Ordering::Relaxed);
            }
            "resize_scale" if self.resize_scale().is_some() => {
                let s = num(0.5, 4.0)?;
                self.hub
                    .hello_extra
                    .lock()
                    .insert("resize".into(), json!({ "scale": s }));
            }
            "auto_resize" if self.resize_scale().is_some() => {
                self.auto_resize.store(flag()?, Ordering::Relaxed);
            }
            // A fixed size from the panel's menu; the desktop stops following windows.
            "resolution" if self.resize_scale().is_some() => {
                let size = v
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
                    .filter(|s| self.hub.resize_sizes.lock().contains(s))
                    .ok_or_else(|| format!("{v} isn't one of this display's resolutions"))?;
                self.auto_resize.store(false, Ordering::Relaxed);
                self.hub.request_size(size);
            }
            _ => return Err(format!("{key} can't be changed while voxl runs")),
        }
        log::info!("setting {key} = {v}");
        Ok(())
    }

    /// New viewers' hello says which transfers the Files panel offers.
    fn update_files_hello(&self) {
        let mut extra = self.hub.hello_extra.lock();
        if let Some(Value::Object(f)) = extra.get_mut("files") {
            f.insert("upload".into(), self.files.can_upload().into());
            f.insert("download".into(), self.files.can_download().into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn settings(upload: bool, max_upload_bytes: u64) -> Arc<Settings> {
        let hub = Hub::new();
        *hub.video_cfg.lock() = Some(crate::video::VideoConfig {
            display: None,
            fps: 60,
            preset: "ultrafast".into(),
            threads: 2,
            convert_threads: 4,
            crf: 24.0,
            max_kbps: 30000,
            paintover_crf: 16.0,
            paintover_burst: 3,
            paintover_delay_ms: 150,
            min_kbps: 2000,
            coalesce: Duration::ZERO,
            stripes: 1,
            adaptive_threads: false,
            auto_quality: true,
        });
        hub.audio_bitrate.store(128000, Ordering::Relaxed);
        hub.audio_auto.store(128000, Ordering::Relaxed);
        let files = Arc::new(Files {
            root: std::env::temp_dir(),
            upload,
            download: true,
            default_dir: String::new(),
            max_upload_bytes,
            live: Default::default(),
        });
        let recorder = Recorder::new(hub.clone(), Duration::from_secs(300));
        Settings::new(hub, files, recorder, true, false, true)
    }

    #[test]
    fn video_settings_reach_the_pipeline() {
        let s = settings(true, 0);
        s.set("crf", "30").unwrap();
        s.set("framerate", "30").unwrap();
        s.set("preset", "veryfast").unwrap();
        let c = s.hub.video_cfg.lock().clone().unwrap();
        assert_eq!((c.crf, c.fps, c.preset.as_str()), (30.0, 30, "veryfast"));
        assert!(s.hub.video_dirty.load(Ordering::Relaxed));
        // The floor never ends up above the ceiling.
        s.set("max_kbps", "1000").unwrap();
        let c = s.hub.video_cfg.lock().clone().unwrap();
        assert_eq!((c.max_kbps, c.min_kbps), (1000, 1000));
    }

    #[test]
    fn picking_a_quality_turns_auto_off_and_auto_restores_the_start() {
        let s = settings(true, 0);
        s.set("crf", "30").unwrap();
        s.set("max_kbps", "4000").unwrap();
        let c = s.hub.video_cfg.lock().clone().unwrap();
        assert!(!c.auto_quality);
        s.set("auto_quality", "true").unwrap();
        let c = s.hub.video_cfg.lock().clone().unwrap();
        assert!(c.auto_quality);
        assert_eq!((c.crf, c.max_kbps, c.min_kbps), (24.0, 30000, 2000));
    }

    #[test]
    fn playback_autos_are_on_and_picking_a_value_overrides_them() {
        let s = settings(true, 0);
        let m: Value = serde_json::from_str(&s.message()).unwrap();
        assert_eq!(m["values"]["auto_audio"], true);
        assert_eq!(m["values"]["auto_framerate"], true);
        s.set("audio_bitrate", "64000").unwrap();
        s.set("framerate", "30").unwrap();
        let m: Value = serde_json::from_str(&s.message()).unwrap();
        assert_eq!(m["values"]["auto_audio"], false);
        assert_eq!(m["values"]["auto_framerate"], false);
        s.set("auto_audio", "true").unwrap();
        s.set("auto_framerate", "true").unwrap();
        assert_eq!(s.hub.audio_bitrate.load(Ordering::Relaxed), 128000);
        assert_eq!(s.hub.video_cfg.lock().as_ref().unwrap().fps, 60);
    }

    #[test]
    fn bad_values_are_refused() {
        let s = settings(true, 0);
        for (k, v) in [
            ("crf", "99"),
            ("crf", "NaN"),
            ("framerate", "0"),
            ("preset", "placebo"),
            ("audio_bitrate", "1"),
            ("upload", "maybe"),
            ("resize_scale", "2"),
            ("basic_auth_password", "x"),
            ("files_root", "/"),
        ] {
            assert!(s.set(k, v).is_err(), "{k}={v} was accepted");
        }
        let c = s.hub.video_cfg.lock().clone().unwrap();
        assert_eq!((c.crf, c.fps), (24.0, 60));
    }

    #[test]
    fn permissions_only_narrow() {
        let s = settings(false, 10 << 20);
        // --enable-clipboard=in: turning paste-out on is refused, paste-in can go off and back.
        assert!(s.set("clipboard_out", "true").is_err());
        s.set("clipboard_in", "false").unwrap();
        assert!(!s.clip_in());
        s.set("clipboard_in", "true").unwrap();
        assert!(s.clip_in());
        // --file-transfers without upload stays without it.
        assert!(s.set("upload", "true").is_err());
        s.set("download", "false").unwrap();
        assert!(!s.files.can_download());
        // --max-upload-mb=10: lower is fine, higher or unlimited is not.
        s.set("max_upload_mb", "4").unwrap();
        assert_eq!(s.files.upload_limit(), 4 << 20);
        assert!(s.set("max_upload_mb", "11").is_err());
        assert!(s.set("max_upload_mb", "0").is_err());
        // --max-record-seconds=300 is the longest.
        s.set("max_record_seconds", "60").unwrap();
        assert_eq!(s.recorder.max_len(), Duration::from_secs(60));
        assert!(s.set("max_record_seconds", "301").is_err());
    }

    #[test]
    fn resolution_needs_dynamic_resolution_and_a_listed_size() {
        let s = settings(true, 0);
        // Without --enable-resize, neither can change.
        assert!(s.set("resolution", "1280x720").is_err());
        assert!(s.set("auto_resize", "false").is_err());
        s.hub
            .hello_extra
            .lock()
            .insert("resize".into(), json!({ "scale": 1.5 }));
        *s.hub.resize_sizes.lock() = vec![(1280, 720), (1920, 1080)];
        s.hub.width.store(1920, Ordering::Relaxed);
        s.hub.height.store(1080, Ordering::Relaxed);
        assert!(s.auto_resize());
        // Picking a size asks for it and stops following windows.
        s.set("resolution", "1280x720").unwrap();
        assert_eq!(*s.hub.resize_req.lock(), Some((1280, 720)));
        assert!(!s.auto_resize());
        // While that change is under way the desktop still says 1920x1080; going back
        // to it must still go through (a page on the screen it shows does just that).
        s.set("resolution", "1920x1080").unwrap();
        assert_eq!(*s.hub.resize_req.lock(), Some((1920, 1080)));
        // Asking again for the size it is already changing to is a no-op.
        s.hub.resize_req.lock().take();
        s.set("resolution", "1920x1080").unwrap();
        assert_eq!(*s.hub.resize_req.lock(), None);
        assert!(s.set("resolution", "1000x700").is_err());
        assert!(s.set("resolution", "big").is_err());
        s.set("auto_resize", "true").unwrap();
        assert!(s.auto_resize());
        let m: Value = serde_json::from_str(&s.message()).unwrap();
        assert_eq!(m["values"]["resolution"], "1920x1080");
        assert_eq!(m["limits"]["resolutions"][0], "1280x720");
    }

    #[test]
    fn message_lists_values_and_limits() {
        let s = settings(true, 0);
        s.set("audio_bitrate", "64000").unwrap();
        let m: Value = serde_json::from_str(&s.message()).unwrap();
        assert_eq!(m["t"], "settings");
        assert_eq!(m["values"]["audio_bitrate"], 64000);
        assert_eq!(m["values"]["crf"], 24.0);
        assert_eq!(m["limits"]["clipboard_out"], false);
        assert_eq!(m["limits"]["resize"], false);
        assert!(m["restart"].as_array().unwrap().contains(&json!("preset")));
    }
}
