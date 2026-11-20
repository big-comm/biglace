//! Which H.264 encoder a screen share runs. Hardware first; each candidate
//! must encode a test frame before it is used, and a software encoder always
//! stays in reserve.

use std::path::Path;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoder {
    /// VA-API: Intel and AMD.
    VaH264,
    /// NVENC, on NVIDIA's own driver.
    NvH264,
    X264,
    OpenH264,
}

impl Encoder {
    /// Every encoder, in order of preference.
    pub(super) const ALL: [Encoder; 4] = [Encoder::VaH264, Encoder::NvH264, Encoder::X264, Encoder::OpenH264];

    pub(crate) fn element(self) -> &'static str {
        match self {
            Encoder::VaH264 => "vah264enc",
            Encoder::NvH264 => "nvh264enc",
            Encoder::X264 => "x264enc",
            Encoder::OpenH264 => "openh264enc",
        }
    }

    fn is_hardware(self) -> bool {
        matches!(self, Encoder::VaH264 | Encoder::NvH264)
    }

    /// The raw format the encoder takes. NVENC converts BGRx on the card, so
    /// a screen's frames need no conversion on the CPU.
    pub(crate) fn format(self) -> &'static str {
        match self {
            Encoder::VaH264 => "NV12",
            Encoder::NvH264 => "BGRx",
            // Full chroma: colored text edges stay sharp (what RDP's AVC444 is for).
            Encoder::X264 => "Y444",
            Encoder::OpenH264 => "I420",
        }
    }

    /// The element with its low-latency settings: no B-frames, one
    /// reference, a keyframe at least every 120 frames.
    pub(crate) fn description(self) -> String {
        match self {
            // Slices let the viewer's decoder work on one frame in parallel.
            Encoder::VaH264 => "vah264enc name=enc rate-control=cqp qpi=20 qpp=22 key-int-max=120 b-frames=0 ref-frames=1 \
                 target-usage=4 num-slices=4"
                .to_string(),
            Encoder::NvH264 => "nvh264enc name=enc preset=low-latency-hq rc-mode=cbr bitrate=30000 gop-size=120 bframes=0 \
                 zerolatency=true"
                .to_string(),
            // At most half the cores: the person at this computer keeps using
            // it while it's shared. Constant quality (CRF 20: crisp text)
            // capped at 30 Mbit/s with a short VBV.
            Encoder::X264 => {
                let threads = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(1, 4));
                format!(
                    "x264enc name=enc tune=zerolatency speed-preset=ultrafast sliced-threads=true threads={threads} \
                     pass=qual quantizer=20 bitrate=30000 vbv-buf-capacity=100 key-int-max=120 bframes=0 b-adapt=false"
                )
            }
            Encoder::OpenH264 => "openh264enc name=enc bitrate=6000000 gop-size=60 complexity=low".to_string(),
        }
    }
}

/// The encoders worth trying on this machine, best first: NVENC only on
/// NVIDIA's own driver, and VA-API never there (it does not work under it).
fn candidates(installed: impl Fn(&str) -> bool, nvidia: bool) -> Vec<Encoder> {
    Encoder::ALL
        .into_iter()
        .filter(|e| installed(e.element()))
        .filter(|e| match e {
            Encoder::VaH264 => !nvidia,
            Encoder::NvH264 => nvidia,
            Encoder::X264 | Encoder::OpenH264 => true,
        })
        .collect()
}

/// The candidates that `works`, up to and including the first software one:
/// a hardware encoder can still refuse a capture's size or format later.
fn working(candidates: Vec<Encoder>, works: impl Fn(Encoder) -> bool) -> Vec<Encoder> {
    let mut found = Vec::new();
    for encoder in candidates {
        if !works(encoder) {
            crate::tailscale::dbg(&format!("remote: {} failed to encode a test frame", encoder.element()));
            continue;
        }
        found.push(encoder);
        if !encoder.is_hardware() {
            break;
        }
    }
    found
}

/// Whether the primary GPU (the first `cardN` with a driver) runs NVIDIA's
/// own driver. `drm` is `/sys/class/drm`.
fn nvidia_driver(drm: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(drm) else { return false };
    let mut cards: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.strip_prefix("card").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())))
        .collect();
    cards.sort();
    cards
        .iter()
        .find_map(|card| std::fs::read_link(drm.join(card).join("device/driver")).ok())
        .is_some_and(|driver| driver.file_name().is_some_and(|m| m == "nvidia"))
}

/// Encode one frame with `encoder`'s own settings.
fn encodes_a_frame(encoder: Encoder) -> bool {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    let desc = format!(
        "videotestsrc num-buffers=1 ! video/x-raw,format={},width=640,height=480 ! {} ! appsink name=sink sync=false",
        encoder.format(),
        encoder.description()
    );
    let Ok(pipeline) = gst::parse::launch(&desc).map(|p| p.downcast::<gst::Pipeline>()) else { return false };
    let Ok(pipeline) = pipeline else { return false };
    let Some(sink) = pipeline.by_name("sink").and_then(|e| e.downcast::<gstreamer_app::AppSink>().ok()) else {
        return false;
    };
    let encoded = pipeline.set_state(gst::State::Playing).is_ok() && sink.try_pull_sample(gst::ClockTime::from_seconds(5)).is_some();
    let _ = pipeline.set_state(gst::State::Null);
    encoded
}

/// The encoders that work here, best first, with their descriptions
/// (probed once).
pub(crate) fn usable() -> &'static [(Encoder, String)] {
    static USABLE: OnceLock<Vec<(Encoder, String)>> = OnceLock::new();
    USABLE.get_or_init(|| {
        if gstreamer::init().is_err() {
            return Vec::new();
        }
        let installed = |name: &str| gstreamer::ElementFactory::find(name).is_some();
        let found = working(candidates(installed, nvidia_driver(Path::new("/sys/class/drm"))), encodes_a_frame);
        crate::tailscale::dbg(&format!("remote: H.264 encoders {found:?}"));
        found.into_iter().map(|e| (e, e.description())).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_decides_hardware_encoder() {
        let all = |_: &str| true;
        assert_eq!(candidates(all, true), [Encoder::NvH264, Encoder::X264, Encoder::OpenH264]);
        assert_eq!(candidates(all, false), [Encoder::VaH264, Encoder::X264, Encoder::OpenH264]);
        assert_eq!(candidates(|name| name == "openh264enc", true), [Encoder::OpenH264]);
        assert!(candidates(|_| false, false).is_empty());
    }

    /// A hardware encoder that fails its test frame gives way, and software
    /// stays behind a working one.
    #[test]
    fn failing_encoder_falls_back() {
        let all = vec![Encoder::VaH264, Encoder::X264, Encoder::OpenH264];
        assert_eq!(working(all.clone(), |e| e != Encoder::VaH264), [Encoder::X264]);
        assert_eq!(working(all.clone(), |_| true), [Encoder::VaH264, Encoder::X264]);
        assert_eq!(working(all.clone(), |e| e == Encoder::OpenH264), [Encoder::OpenH264]);
        assert!(working(all, |_| false).is_empty());
    }

    #[test]
    fn the_first_card_with_a_driver_decides() {
        let root = std::env::temp_dir().join(format!("biglace-drm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let card = |name: &str, driver: Option<&str>| {
            let device = root.join(name).join("device");
            std::fs::create_dir_all(&device).unwrap();
            if let Some(driver) = driver {
                std::os::unix::fs::symlink(format!("../../../bus/pci/drivers/{driver}"), device.join("driver")).unwrap();
            }
        };
        assert!(!nvidia_driver(&root));
        card("card0", None);
        card("card0-HDMI-A-1", Some("nvidia"));
        card("card1", Some("nvidia"));
        card("renderD128", Some("i915"));
        assert!(nvidia_driver(&root));
        card("card0", Some("i915"));
        assert!(!nvidia_driver(&root));
        let _ = std::fs::remove_dir_all(&root);
    }
}
