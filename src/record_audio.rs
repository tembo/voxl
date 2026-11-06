//! A separate continuous Opus encoder for recordings. Live audio keeps its own encoder
//! and can continue skipping silence without desynchronizing its decoder.
use crate::hub::{Hub, Out};
use bytes::Bytes;
use std::ffi::{c_int, c_void};
use std::sync::atomic::Ordering;
use std::time::Instant;

#[cfg_attr(not(windows), link(name = "opus"))]
extern "C" {
    fn opus_encoder_create(
        rate: i32,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut c_void;
    fn opus_encoder_destroy(encoder: *mut c_void);
    fn opus_encoder_ctl(encoder: *mut c_void, request: c_int, ...) -> c_int;
    fn opus_encode(
        encoder: *mut c_void,
        pcm: *const i16,
        samples: c_int,
        data: *mut u8,
        max: i32,
    ) -> c_int;
}

pub struct Encoder {
    encoder: *mut c_void,
    bitrate: i32,
    buffer: [u8; 4000],
}
// The encoder is used by only one capture thread or under the macOS callback mutex.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(bitrate: i32) -> Self {
        Self {
            encoder: std::ptr::null_mut(),
            bitrate,
            buffer: [0; 4000],
        }
    }

    pub fn push(&mut self, hub: &Hub, pcm: &[i16]) {
        let clients: Vec<_> = hub
            .clients
            .lock()
            .iter()
            .filter(|c| c.recording_audio.load(Ordering::Relaxed))
            .cloned()
            .collect();
        if clients.is_empty() {
            self.reset();
            return;
        }
        if self.encoder.is_null() {
            let mut error = 0;
            self.encoder = unsafe { opus_encoder_create(48000, 2, 2051, &mut error) };
            if self.encoder.is_null() {
                log::error!("recording Opus encoder init failed: {error}");
                return;
            }
            unsafe { opus_encoder_ctl(self.encoder, 4002, self.bitrate as c_int) };
        }
        let n = unsafe {
            opus_encode(
                self.encoder,
                pcm.as_ptr(),
                480,
                self.buffer.as_mut_ptr(),
                self.buffer.len() as i32,
            )
        };
        if n <= 0 {
            return;
        }
        let data = Bytes::copy_from_slice(&self.buffer[..n as usize]);
        let captured = Instant::now()
            .checked_sub(std::time::Duration::from_millis(10))
            .unwrap_or_else(Instant::now);
        for client in clients {
            client.send(Out::RecordingAudio(data.clone(), captured));
        }
    }

    fn reset(&mut self) {
        if !self.encoder.is_null() {
            unsafe { opus_encoder_destroy(self.encoder) };
            self.encoder = std::ptr::null_mut();
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn silence_goes_to_recorder_not_live_viewer() {
        let hub = Hub::new();
        let (tx, mut recording) = mpsc::unbounded_channel();
        let recorder = hub.add_client(tx);
        recorder.recording_audio.store(true, Ordering::Relaxed);
        let (tx, mut live) = mpsc::unbounded_channel();
        let viewer = hub.add_client(tx);
        viewer.audio.store(true, Ordering::Relaxed);
        let mut encoder = Encoder::new(128_000);
        encoder.push(&hub, &[0; 960]);
        assert!(matches!(
            recording.try_recv(),
            Ok(Out::RecordingAudio(_, _))
        ));
        assert!(live.try_recv().is_err());
    }
}
