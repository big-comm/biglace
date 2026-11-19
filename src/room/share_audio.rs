//! The computer's sound, shared with the room along with the screen —
//! without the room's own voices (they'd come back to everyone as an echo).
//!
//! While it's on, the other programs play into a sink of our own
//! (`biglace_share`), looped back to the real speakers so the person still
//! hears them; that sink's monitor is what's sent. BigLace's own streams
//! (the room's voices) stay on the real speakers and aren't captured.
//! Everything is put back when it ends (or at the next start, after a
//! crash).

use std::process::Command;
use std::sync::{Mutex, OnceLock};

use serde::Deserialize;

const SINK: &str = "biglace_share";

/// What we changed, to undo it.
struct Active {
    original_sink: String,
    modules: Vec<String>,
}

fn active() -> &'static Mutex<Option<Active>> {
    static A: OnceLock<Mutex<Option<Active>>> = OnceLock::new();
    A.get_or_init(Default::default)
}

fn pactl(args: &[&str]) -> Option<String> {
    let out = Command::new("pactl").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[derive(Deserialize)]
struct SinkInput {
    index: u64,
    #[serde(default)]
    sink: u64,
    #[serde(default)]
    properties: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct Sink {
    index: u64,
    name: String,
}

fn sink_inputs() -> Vec<SinkInput> {
    pactl(&["-f", "json", "list", "sink-inputs"]).and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default()
}

fn sink_index(name: &str) -> Option<u64> {
    let sinks: Vec<Sink> = pactl(&["-f", "json", "list", "sinks"]).and_then(|j| serde_json::from_str(&j).ok())?;
    sinks.into_iter().find(|s| s.name == name).map(|s| s.index)
}

/// A stream of this BigLace (the room's voices) — left where it is.
fn ours(i: &SinkInput) -> bool {
    let pid = std::process::id().to_string();
    let prop = |k: &str| i.properties.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
    prop("application.process.id") == pid || prop("media.name").starts_with("loopback") || prop("node.name").contains("loopback")
}

/// The real speakers, while the sound is being shared (room voices go
/// there explicitly).
pub fn real_sink() -> Option<String> {
    active().lock().ok()?.as_ref().map(|a| a.original_sink.clone())
}

/// Leftovers of a share that didn't end cleanly (BigLace was killed).
pub fn clean_leftovers() {
    let Some(list) = pactl(&["list", "short", "modules"]) else { return };
    for line in list.lines() {
        if line.contains(SINK) {
            if let Some(id) = line.split_whitespace().next() {
                let _ = pactl(&["unload-module", id]);
            }
        }
    }
}

/// Start sending the computer's sound apart from the room's voices.
/// Returns the capture source to read from.
pub fn start() -> Result<String, String> {
    if active().lock().map(|a| a.is_some()).unwrap_or(true) {
        return Ok(format!("{SINK}.monitor"));
    }
    clean_leftovers();
    let original = pactl(&["get-default-sink"]).filter(|s| !s.is_empty()).ok_or("no sound output")?;
    let mut modules = Vec::new();
    let undo = |modules: &[String]| {
        for m in modules.iter().rev() {
            let _ = pactl(&["unload-module", m]);
        }
    };
    let null = pactl(&[
        "load-module",
        "module-null-sink",
        &format!("sink_name={SINK}"),
        "sink_properties=device.description=BigLace",
    ])
    .ok_or("could not create the sound share")?;
    modules.push(null);
    match pactl(&[
        "load-module",
        "module-loopback",
        &format!("source={SINK}.monitor"),
        &format!("sink={original}"),
        "latency_msec=30",
    ]) {
        Some(m) => modules.push(m),
        None => {
            undo(&modules);
            return Err("could not keep the sound on the speakers".into());
        }
    }
    // The room's voices are pinned to the speakers first: PipeWire moves
    // every stream without a target along with the default sink.
    for i in sink_inputs().iter().filter(|i| ours(i)) {
        let _ = pactl(&["move-sink-input", &i.index.to_string(), &original]);
    }
    let _ = pactl(&["set-default-sink", SINK]);
    // What's playing now moves into the share (not the room's voices).
    for i in sink_inputs() {
        let target = if ours(&i) { original.as_str() } else { SINK };
        let _ = pactl(&["move-sink-input", &i.index.to_string(), target]);
    }
    if let Ok(mut a) = active().lock() {
        *a = Some(Active { original_sink: original, modules });
    }
    Ok(format!("{SINK}.monitor"))
}

/// Put everything back.
pub fn stop() {
    let Some(a) = active().lock().ok().and_then(|mut a| a.take()) else { return };
    let _ = pactl(&["set-default-sink", &a.original_sink]);
    if let Some(ours_idx) = sink_index(SINK) {
        for i in sink_inputs() {
            if i.sink == ours_idx {
                let _ = pactl(&["move-sink-input", &i.index.to_string(), &a.original_sink]);
            }
        }
    }
    for m in a.modules.iter().rev() {
        let _ = pactl(&["unload-module", m]);
    }
}

/// The capture pipeline for `source` (Opus, like a remote session's sound).
pub fn capture_desc(source: &str) -> String {
    format!(
        "pulsesrc name=asrc device={source} buffer-time=40000 latency-time=10000 \
         ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 \
         ! opusenc bitrate=128000 frame-size=10 audio-type=generic \
         ! appsink name=asink sync=false max-buffers=50"
    )
}
