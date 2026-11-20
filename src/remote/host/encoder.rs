//! Which H.264 encoder a screen share runs, and the bitrate it is held to.
//! Hardware first; each candidate must encode a test frame before it is used,
//! and a software encoder always stays in reserve.

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

    pub(crate) fn from_element(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.element() == name)
    }

    /// The bitrate a share starts at, and returns to when the link allows.
    pub(crate) fn ceiling_kbps(self) -> u32 {
        match self {
            Encoder::OpenH264 => 6_000,
            Encoder::VaH264 | Encoder::NvH264 | Encoder::X264 => 30_000,
        }
    }

    /// The element with its low-latency settings (no B-frames, one
    /// reference, a keyframe at least every 120 frames) at its ceiling.
    pub(crate) fn description(self) -> String {
        let kbps = self.ceiling_kbps();
        let vbv = vbv_kbits(kbps);
        match self {
            // Slices let the viewer's decoder work on one frame in parallel.
            Encoder::VaH264 => format!(
                "vah264enc name=enc rate-control=cbr bitrate={kbps} cpb-size={vbv} key-int-max=120 b-frames=0 ref-frames=1 \
                 target-usage=4 num-slices=4"
            ),
            Encoder::NvH264 => format!(
                "nvh264enc name=enc preset=p4 tune=low-latency rc-mode=cbr bitrate={kbps} vbv-buffer-size={vbv} gop-size=120 \
                 bframes=0 zerolatency=true"
            ),
            // At most half the cores: the person at this computer keeps using
            // it while it's shared. Constant quality (CRF 20: crisp text),
            // with the bitrate as its cap.
            Encoder::X264 => {
                let threads = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(1, 4));
                format!(
                    "x264enc name=enc tune=zerolatency speed-preset=ultrafast sliced-threads=true threads={threads} \
                     pass=qual quantizer=20 bitrate={kbps} vbv-buf-capacity={VBV_MS} key-int-max=120 bframes=0 b-adapt=false"
                )
            }
            // Frame skipping is what holds openh264 to its bitrate.
            Encoder::OpenH264 => format!(
                "openh264enc name=enc rate-control=bitrate bitrate={bps} max-bitrate={bps} enable-frame-skip=true \
                 gop-size=60 complexity=low",
                bps = kbps * 1000
            ),
        }
    }

    /// Set a running encoder (`element`, built from [`Self::description`])
    /// to `kbps`.
    pub(crate) fn retune(self, element: &gstreamer::Element, kbps: u32) {
        use gstreamer::prelude::*;
        match self {
            Encoder::VaH264 => {
                element.set_property("bitrate", kbps);
                element.set_property("cpb-size", vbv_kbits(kbps));
            }
            Encoder::NvH264 => {
                element.set_property("bitrate", kbps);
                element.set_property("vbv-buffer-size", vbv_kbits(kbps));
            }
            Encoder::X264 => element.set_property("bitrate", kbps),
            Encoder::OpenH264 => {
                element.set_property("bitrate", kbps.saturating_mul(1000));
                element.set_property("max-bitrate", kbps.saturating_mul(1000));
            }
        }
    }
}

/// The encoder's buffer: 3 frames at 60 fps, so a frame bigger than the link
/// carries is cut down instead of queued.
const VBV_MS: u32 = 50;

fn vbv_kbits(kbps: u32) -> u32 {
    (kbps * VBV_MS / 1000).max(1)
}

/// Holds the bitrate to what the viewer's link carries: the socket's send
/// buffer filling up (sending blocks) is the sign of a link slower than the
/// video. Optimistic start, quick retreat, slow return.
pub(crate) struct RateControl {
    ceiling: u32,
    current: u32,
    healthy_windows: u32,
}

/// How long one window of sending is.
pub(crate) const RATE_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);
/// Never below this, whatever the link does.
const FLOOR_KBPS: u32 = 300;
/// Windows of free sending before the rate goes up again, so a quiet moment
/// in a bad patch does not undo the retreat.
const HEALTHY_WINDOWS_BEFORE_RAISING: u32 = 4;

impl RateControl {
    pub(crate) fn new(ceiling_kbps: u32) -> Self {
        Self { ceiling: ceiling_kbps, current: ceiling_kbps, healthy_windows: 0 }
    }

    /// One window: the socket took `sent` bytes in `window`, `blocked` of it
    /// spent waiting for room. The new rate, when it changed.
    pub(crate) fn observe(&mut self, sent: u64, blocked: std::time::Duration, window: std::time::Duration) -> Option<u32> {
        let previous = self.current;
        if blocked * 4 >= window {
            self.healthy_windows = 0;
            // What the link carried, with room for the queue to drain. A
            // backlog still draining at that rate is no reason to go lower.
            let millis = u64::try_from(window.as_millis()).unwrap_or(u64::MAX).max(1);
            let carried = u32::try_from(sent * 8 / millis).unwrap_or(u32::MAX);
            self.current = (carried / 5 * 4).min(self.current).max(FLOOR_KBPS);
        } else {
            self.healthy_windows += 1;
            if self.healthy_windows >= HEALTHY_WINDOWS_BEFORE_RAISING && self.current < self.ceiling {
                self.healthy_windows = 0;
                self.current = (self.current / 10 * 11).min(self.ceiling);
            }
        }
        (self.current != previous).then_some(self.current)
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
fn working(candidates: Vec<Encoder>, works: impl Fn(Encoder) -> anyhow::Result<()>) -> Vec<Encoder> {
    let mut found = Vec::new();
    for encoder in candidates {
        if let Err(e) = works(encoder) {
            crate::tailscale::dbg(&format!("remote: {} failed to encode a test frame: {e:#}", encoder.element()));
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
fn encodes_a_frame(encoder: Encoder) -> anyhow::Result<()> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    let desc = format!(
        "videotestsrc num-buffers=1 ! video/x-raw,format={},width=640,height=480 ! {} ! appsink name=sink sync=false",
        encoder.format(),
        encoder.description()
    );
    let pipeline = gst::parse::launch(&desc)?.downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("not a pipeline"))?;
    let sink = pipeline
        .by_name("sink")
        .and_then(|e| e.downcast::<gstreamer_app::AppSink>().ok())
        .ok_or_else(|| anyhow::anyhow!("no appsink"))?;
    let encoded = first_sample(&pipeline, &sink);
    let _ = pipeline.set_state(gstreamer::State::Null);
    encoded
}

/// Waits for `sink`'s first sample, giving up at the pipeline's first error
/// instead of at the deadline.
fn first_sample(pipeline: &gstreamer::Pipeline, sink: &gstreamer_app::AppSink) -> anyhow::Result<()> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    let bus = pipeline.bus().ok_or_else(|| anyhow::anyhow!("no bus"))?;
    let error = || {
        let msg = bus.pop_filtered(&[gst::MessageType::Error])?;
        match msg.view() {
            gst::MessageView::Error(e) => Some(anyhow::anyhow!("{}", e.error())),
            _ => None,
        }
    };
    if pipeline.set_state(gst::State::Playing).is_err() {
        return Err(error().unwrap_or_else(|| anyhow::anyhow!("could not start")));
    }
    let deadline = std::time::Instant::now() + PROBE_WITHIN;
    while std::time::Instant::now() < deadline {
        if sink.try_pull_sample(gst::ClockTime::from_mseconds(100)).is_some() {
            return Ok(());
        }
        if let Some(e) = error() {
            return Err(e);
        }
    }
    anyhow::bail!("no frame within {PROBE_WITHIN:?}")
}

/// A test frame not encoded by then means the encoder does not work here.
const PROBE_WITHIN: std::time::Duration = std::time::Duration::from_secs(5);

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
    use std::time::Duration;

    const WINDOW: Duration = RATE_WINDOW;

    #[test]
    fn fast_link_keeps_the_ceiling() {
        let mut rate = RateControl::new(30_000);
        for _ in 0..20 {
            assert_eq!(rate.observe(1_000_000, Duration::from_millis(10), WINDOW), None);
        }
    }

    /// 2 Mbit/s carried while sending waited half the window: the rate drops
    /// at once below what the link carried, stays there while the backlog
    /// drains, then climbs back a tenth at a time once sending flows freely.
    #[test]
    fn slow_link_cuts_then_recovers() {
        let mut rate = RateControl::new(30_000);
        let carried = 2_000 * 1000 / 8 / 2;
        assert_eq!(rate.observe(carried, WINDOW / 2, WINDOW), Some(1_600));
        assert_eq!(rate.observe(carried, WINDOW, WINDOW), None);
        assert_eq!(rate.observe(carried / 2, WINDOW, WINDOW), Some(800));
        for _ in 0..3 {
            assert_eq!(rate.observe(1_000, Duration::ZERO, WINDOW), None);
        }
        assert_eq!(rate.observe(1_000, Duration::ZERO, WINDOW), Some(880));
        let mut last = 880;
        for _ in 0..200 {
            if let Some(kbps) = rate.observe(1_000, Duration::ZERO, WINDOW) {
                assert!(kbps > last && kbps <= 30_000);
                last = kbps;
            }
        }
        assert_eq!(last, 30_000);
    }

    #[test]
    fn rate_stays_above_floor() {
        let mut rate = RateControl::new(30_000);
        assert_eq!(rate.observe(0, WINDOW, WINDOW), Some(FLOOR_KBPS));
        assert_eq!(rate.observe(0, WINDOW, WINDOW), None);
    }

    /// Each encoder present here, retuned while it runs, holds its output
    /// near the new rate on content that would fill any ceiling.
    #[test]
    fn retuned_encoder_follows_new_rate() {
        use gstreamer as gst;
        use gstreamer::prelude::*;
        let _turn = crate::remote::heavy_test();
        gst::init().unwrap();
        const TARGET: u32 = 1_000;
        // Bytes over the stretch of picture they cover (an encoder may skip frames).
        let kbps_of = |frames: &[(u64, usize)]| {
            let millis = (frames[frames.len() - 1].0 - frames[0].0).max(1);
            frames.iter().map(|f| f.1 as u64).sum::<u64>() * 8 / millis
        };
        for kind in Encoder::ALL.into_iter().filter(|e| gst::ElementFactory::find(e.element()).is_some()) {
            let desc = format!(
                "videotestsrc pattern=snow num-buffers=150 ! video/x-raw,format={},width=640,height=480,framerate=30/1 \
                 ! {} ! appsink name=sink sync=false max-buffers=1",
                kind.format(),
                kind.description()
            );
            let pipeline = gst::parse::launch(&desc).unwrap().downcast::<gst::Pipeline>().unwrap();
            let sink = pipeline.by_name("sink").unwrap().downcast::<gstreamer_app::AppSink>().unwrap();
            let enc = pipeline.by_name("enc").unwrap();
            pipeline.set_state(gst::State::Playing).unwrap();
            // (milliseconds into the picture, bytes)
            let mut frames: Vec<(u64, usize)> = Vec::new();
            let mut start = None;
            while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
                let buffer = sample.buffer().unwrap();
                let pts = buffer.pts().unwrap().mseconds();
                let at = pts - *start.get_or_insert(pts);
                if frames.last().is_some_and(|f| f.0 < 2000) && at >= 2000 {
                    kind.retune(&enc, TARGET);
                }
                frames.push((at, buffer.size()));
            }
            pipeline.set_state(gst::State::Null).unwrap();
            let span = |from: u64, to: u64| frames.iter().copied().filter(|f| (from..to).contains(&f.0)).collect::<Vec<_>>();
            let (before, after) = (kbps_of(&span(500, 2000)), kbps_of(&span(3000, 5000)));
            assert!(before > u64::from(TARGET) * 3, "{kind:?} started at {before} kbit/s");
            assert!(after < u64::from(TARGET) * 3 / 2, "{kind:?} stayed at {after} kbit/s");
        }
    }

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
        let only = |pass: bool| if pass { Ok(()) } else { Err(anyhow::anyhow!("refused")) };
        assert_eq!(working(all.clone(), |e| only(e != Encoder::VaH264)), [Encoder::X264]);
        assert_eq!(working(all.clone(), |_| only(true)), [Encoder::VaH264, Encoder::X264]);
        assert_eq!(working(all.clone(), |e| only(e == Encoder::OpenH264)), [Encoder::OpenH264]);
        assert!(working(all, |_| only(false)).is_empty());
    }

    /// A pipeline whose element fails on the first frame reports why at
    /// once, well before the probe's deadline.
    #[test]
    fn failing_probe_returns_the_reason_early() {
        use gstreamer as gst;
        use gstreamer::prelude::*;
        gst::init().unwrap();
        let desc = "videotestsrc num-buffers=1 ! identity error-after=1 ! appsink name=sink";
        let pipeline = gst::parse::launch(desc).unwrap().downcast::<gst::Pipeline>().unwrap();
        let sink = pipeline.by_name("sink").unwrap().downcast::<gstreamer_app::AppSink>().unwrap();
        let started = std::time::Instant::now();
        let reason = first_sample(&pipeline, &sink).unwrap_err().to_string();
        let _ = pipeline.set_state(gst::State::Null);
        assert!(started.elapsed() < PROBE_WITHIN / 2, "took {:?}", started.elapsed());
        assert!(!reason.starts_with("no frame within"), "{reason}");
    }

    #[test]
    fn first_card_driver_decides() {
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
