//! Voice messages: record from the microphone into a small Opus file, with
//! the loudness along the way (drawn as the message's sound bars).

use std::path::PathBuf;
use std::time::Instant;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Microphone → Opus in Ogg (what the other side plays).
const RECORD_DESC: &str = "pulsesrc \
     ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=1 \
     ! level name=level interval=50000000 post-messages=true \
     ! opusenc bitrate=32000 audio-type=voice \
     ! oggmux ! filesink name=sink";
/// Bars drawn for a voice message.
pub const WAVE_BARS: usize = 48;
/// Longest voice message.
pub const MAX_MS: u64 = 15 * 60 * 1000;

pub struct Recorder {
    pipeline: gst::Pipeline,
    path: PathBuf,
    started: Instant,
    levels: Vec<f32>,
    done: bool,
}

impl Recorder {
    pub fn start() -> Result<Self, String> {
        gst::init().map_err(|e| e.to_string())?;
        let dir = super::chat_dir().join("media");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(format!("voice-{}.ogg", super::new_id()));
        let pipeline = gst::parse::launch(RECORD_DESC)
            .map_err(|e| e.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "not a pipeline".to_string())?;
        let sink = pipeline.by_name("sink").ok_or("no sink")?;
        sink.set_property("location", path.to_string_lossy().as_ref());
        pipeline.set_state(gst::State::Playing).map_err(|e| e.to_string())?;
        Ok(Self { pipeline, path, started: Instant::now(), levels: Vec::new(), done: false })
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// New loudness readings (0–1); the latest, if any. Also reports a
    /// broken microphone.
    pub fn poll(&mut self) -> Result<Option<f32>, String> {
        let Some(bus) = self.pipeline.bus() else { return Ok(None) };
        let mut latest = None;
        while let Some(msg) = bus.pop() {
            match msg.view() {
                gst::MessageView::Element(e) => {
                    if let Some(level) = e.structure().filter(|s| s.name() == "level").and_then(crate::room::loudness) {
                        self.levels.push(level);
                        latest = Some(level);
                    }
                }
                gst::MessageView::Error(e) => return Err(e.error().to_string()),
                _ => {}
            }
        }
        Ok(latest)
    }

    /// Stop and keep it: (file, length, sound bars).
    pub fn finish(mut self) -> Result<(PathBuf, u64, Vec<u8>), String> {
        let duration = self.elapsed_ms();
        let _ = self.poll();
        // End of stream first, so the file is closed properly.
        self.pipeline.send_event(gst::event::Eos::new());
        if let Some(bus) = self.pipeline.bus() {
            let _ = bus.timed_pop_filtered(gst::ClockTime::from_seconds(2), &[gst::MessageType::Eos, gst::MessageType::Error]);
        }
        let _ = self.pipeline.set_state(gst::State::Null);
        self.done = true;
        let size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if size == 0 {
            let _ = std::fs::remove_file(&self.path);
            return Err(crate::tr!("nothing was recorded — is a microphone connected?"));
        }
        Ok((self.path.clone(), duration, wave(&self.levels)))
    }

    /// Stop and throw it away.
    pub fn cancel(mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        self.done = true;
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.pipeline.set_state(gst::State::Null);
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Loudness readings → [`WAVE_BARS`] bars of 0–31 (the loudest of each
/// stretch, so short words still show).
pub fn wave(levels: &[f32]) -> Vec<u8> {
    if levels.is_empty() {
        return vec![0; WAVE_BARS];
    }
    (0..WAVE_BARS)
        .map(|i| {
            let a = i * levels.len() / WAVE_BARS;
            let b = ((i + 1) * levels.len() / WAVE_BARS).max(a + 1).min(levels.len());
            let peak = levels[a.min(levels.len() - 1)..b].iter().copied().fold(0.0f32, f32::max);
            (peak.clamp(0.0, 1.0) * 31.0).round() as u8
        })
        .collect()
}

/// "1:05".
pub fn clock(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_follow_the_loudness() {
        assert_eq!(wave(&[]), vec![0; WAVE_BARS]);
        let w = wave(&[0.0, 1.0, 0.5]);
        assert_eq!(w.len(), WAVE_BARS);
        assert!(w.contains(&31) && w.contains(&0));
        let long: Vec<f32> = (0..1000).map(|i| if i < 500 { 0.0 } else { 1.0 }).collect();
        let w = wave(&long);
        assert_eq!(w[0], 0);
        assert_eq!(w[WAVE_BARS - 1], 31);
        assert_eq!(clock(65_000), "1:05");
    }
}
