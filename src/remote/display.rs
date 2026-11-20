//! "Fit the resolution to my window": the host switches its screen to the
//! mode that best fits the viewer's window (among the modes the screen
//! offers), and back to the original when the session ends.
//!
//! One monitor only — with several, their arrangement is the person's, not
//! ours to change. Through the desktop's own tool: `xrandr` (X11),
//! `kscreen-doctor` (KDE), `gdctl` (GNOME).

use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    /// How the tool names it (`1920x1080`, a KDE mode id, `1920x1080@60.000`).
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    Xrandr,
    Kscreen,
    Gdctl,
}

/// The one screen, its modes and the current one.
#[derive(Debug, Clone)]
pub struct Screen {
    tool: Tool,
    output: String,
    /// GNOME: the logical monitor's scale, kept as it is.
    scale: Option<String>,
    pub modes: Vec<Mode>,
    pub current: Option<Mode>,
}

fn has(cmd: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(cmd).is_file()))
}

fn tool() -> Option<Tool> {
    let wayland = super::wayland_session();
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
    if !wayland {
        return has("xrandr").then_some(Tool::Xrandr);
    }
    if desktop.contains("kde") && has("kscreen-doctor") {
        return Some(Tool::Kscreen);
    }
    if desktop.contains("gnome") && has("gdctl") {
        return Some(Tool::Gdctl);
    }
    None
}

/// This computer can fit its resolution to a viewer's window.
pub fn available() -> bool {
    tool().is_some()
}

/// Runs a display tool for at most five seconds: its stdout, or why it failed.
fn run(cmd: &mut Command) -> Result<String, String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let out = crate::tailscale::command_output_timeout(cmd, std::time::Duration::from_secs(5)).map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!("{program}: {} ({})", String::from_utf8_lossy(&out.stderr).trim(), out.status))
    }
}

/// `run`, for a query whose failure means "unknown"; the reason goes to the log.
fn output(cmd: &mut Command) -> Option<String> {
    run(cmd).map_err(|e| crate::tailscale::dbg(&format!("display: {e}"))).ok()
}

fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    let h = h.split(|c: char| !c.is_ascii_digit()).next()?;
    Some((w.trim().parse().ok()?, h.parse().ok()?))
}

/// `xrandr --query` → the single active output.
fn parse_xrandr(text: &str) -> Option<Screen> {
    let mut screens = Vec::new();
    let mut cur: Option<Screen> = None;
    for line in text.lines() {
        if !line.starts_with(' ') {
            if let Some(s) = cur.take() {
                screens.push(s);
            }
            let mut words = line.split_whitespace();
            let name = words.next().unwrap_or_default();
            let rest: Vec<&str> = words.collect();
            // "connected [primary] 1280x800+0+0 …": active when it has a geometry.
            if rest.first() == Some(&"connected") && rest.iter().any(|w| w.contains('+') && w.contains('x')) {
                cur = Some(Screen { tool: Tool::Xrandr, output: name.to_string(), scale: None, modes: vec![], current: None });
            }
            continue;
        }
        let Some(s) = cur.as_mut() else { continue };
        let mut words = line.split_whitespace();
        let Some(id) = words.next() else { continue };
        let Some((width, height)) = parse_size(id) else { continue };
        let mode = Mode { width, height, id: id.to_string() };
        if words.any(|w| w.contains('*')) {
            s.current = Some(mode.clone());
        }
        if !s.modes.iter().any(|m| m.width == width && m.height == height) {
            s.modes.push(mode);
        }
    }
    if let Some(s) = cur {
        screens.push(s);
    }
    (screens.len() == 1).then(|| screens.remove(0))
}

/// `kscreen-doctor -j` → the single enabled output.
fn parse_kscreen(json: &str) -> Option<Screen> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let enabled: Vec<&serde_json::Value> = v
        .get("outputs")?
        .as_array()?
        .iter()
        .filter(|o| o.get("enabled").and_then(serde_json::Value::as_bool) == Some(true))
        .collect();
    let [o] = enabled.as_slice() else { return None };
    let current_id = o.get("currentModeId").map(|c| c.to_string().trim_matches('"').to_string()).unwrap_or_default();
    let mut modes = Vec::new();
    let mut current = None;
    for m in o.get("modes")?.as_array()? {
        let id = m.get("id").map(|c| c.to_string().trim_matches('"').to_string()).unwrap_or_default();
        let size = m.get("size")?;
        let width = size.get("width")?.as_u64()? as u32;
        let height = size.get("height")?.as_u64()? as u32;
        let mode = Mode { width, height, id: id.clone() };
        if id == current_id {
            current = Some(mode.clone());
        }
        modes.push(mode);
    }
    Some(Screen {
        tool: Tool::Kscreen,
        output: o.get("name")?.as_str()?.to_string(),
        scale: None,
        modes,
        current,
    })
}

/// `gdctl show --modes` → the single monitor.
fn parse_gdctl(text: &str) -> Option<Screen> {
    let mut monitors = Vec::new();
    let mut section = "";
    let mut scale = None;
    for raw in text.lines() {
        let line = raw.trim_start_matches(|c: char| c == ' ' || "│├└─".contains(c)).trim();
        if raw.starts_with("Logical monitors") {
            section = "logical";
            continue;
        }
        if let Some(rest) = line.strip_prefix("Monitor ") {
            if section != "logical" {
                let name = rest.split_whitespace().next().unwrap_or_default().to_string();
                monitors.push(Screen { tool: Tool::Gdctl, output: name, scale: None, modes: vec![], current: None });
            }
            section = "monitor";
            continue;
        }
        if line.starts_with("Current mode") {
            section = "current";
            continue;
        }
        if line.starts_with("Modes") {
            section = "modes";
            continue;
        }
        if section == "logical" {
            if let Some(s) = line.strip_prefix("Scale:") {
                scale = Some(s.trim().to_string());
            }
            continue;
        }
        let Some(m) = monitors.last_mut() else { continue };
        let Some((width, height)) = parse_size(line) else {
            if !line.is_empty() && !line.chars().next().is_some_and(|c| c.is_ascii_digit()) && section != "modes" {
                section = "monitor";
            }
            continue;
        };
        let mode = Mode { width, height, id: line.split_whitespace().next().unwrap_or(line).to_string() };
        match section {
            "current" => {
                m.current = Some(mode);
                section = "monitor";
            }
            "modes" => m.modes.push(mode),
            _ => {}
        }
    }
    if monitors.len() != 1 {
        return None;
    }
    let mut s = monitors.remove(0);
    s.scale = scale;
    Some(s)
}

/// The screen as it is now (None: no tool, several monitors…).
pub fn query() -> Option<Screen> {
    match tool()? {
        Tool::Xrandr => parse_xrandr(&output(Command::new("xrandr").arg("--query"))?),
        Tool::Kscreen => parse_kscreen(&output(Command::new("kscreen-doctor").arg("-j"))?),
        Tool::Gdctl => {
            // The modes list leaves out the current one; plain "show" has it.
            let mut screen = parse_gdctl(&output(Command::new("gdctl").args(["show", "--modes"]))?)?;
            let now = parse_gdctl(&output(Command::new("gdctl").arg("show"))?)?;
            screen.current = now.current;
            if screen.scale.is_none() {
                screen.scale = now.scale;
            }
            Some(screen)
        }
    }
}

/// The mode for a `width`×`height` window: the largest that fits in it
/// (else the smallest there is).
pub fn pick(modes: &[Mode], width: u32, height: u32) -> Option<&Mode> {
    let area = |m: &Mode| u64::from(m.width) * u64::from(m.height);
    modes
        .iter()
        .filter(|m| m.width <= width && m.height <= height)
        .max_by_key(|m| area(m))
        .or_else(|| modes.iter().min_by_key(|m| area(m)))
}

pub fn apply(screen: &Screen, mode: &Mode) -> Result<(), String> {
    let mut cmd = match screen.tool {
        Tool::Xrandr => {
            let mut c = Command::new("xrandr");
            c.args(["--output", &screen.output, "--mode", &mode.id]);
            c
        }
        Tool::Kscreen => {
            let mut c = Command::new("kscreen-doctor");
            c.arg(format!("output.{}.mode.{}", screen.output, mode.id));
            c
        }
        Tool::Gdctl => {
            let mut c = Command::new("gdctl");
            c.args(["set", "--logical-monitor", "--primary", "--monitor", &screen.output, "--mode", &mode.id]);
            if let Some(scale) = &screen.scale {
                c.args(["--scale", scale]);
            }
            c
        }
    };
    run(&mut cmd).map(drop)
}

/// X11: a mode made to measure (`cvt`), for screens that list few modes
/// (virtual ones list just one). The name, once added to the output.
fn xrandr_custom(output: &str, width: u32, height: u32) -> Result<String, String> {
    // Widths are kept to multiples of 8 (what drivers expect).
    let width = (width / 8) * 8;
    let text = run(Command::new("cvt").args([width.to_string(), height.to_string(), "60".into()]))?;
    let modeline = text.lines().find_map(|l| l.trim().strip_prefix("Modeline ")).ok_or("cvt: no modeline")?;
    let mut parts = modeline.split_whitespace();
    let name = parts.next().ok_or("cvt: empty modeline")?.trim_matches('"').to_string();
    // A mode left from an earlier session is still there to add.
    if let Err(e) = run(Command::new("xrandr").arg("--newmode").arg(&name).args(parts)) {
        crate::tailscale::dbg(&format!("display: {e}"));
    }
    run(Command::new("xrandr").args(["--addmode", output, &name]))?;
    Ok(name)
}

fn xrandr_forget(output: &str, name: &str) {
    for result in [run(Command::new("xrandr").args(["--delmode", output, name])), run(Command::new("xrandr").args(["--rmmode", name]))] {
        if let Err(e) = result {
            crate::tailscale::dbg(&format!("display: {e}"));
        }
    }
}

/// One session's resolution changes, undone at the end.
#[derive(Default)]
pub struct Fitter {
    original: Option<(Screen, Mode)>,
    /// X11 modes made for this session (removed at the end).
    made: Vec<(String, String)>,
}

impl Fitter {
    /// Fit to a `width`×`height` window. True when the resolution changed.
    pub fn fit(&mut self, width: u32, height: u32) -> Result<bool, String> {
        let screen = query().ok_or("this screen's resolution can't be changed from here")?;
        let mode = pick(&screen.modes, width, height).ok_or("no screen modes")?.clone();
        if self.original.is_none() {
            if let Some(cur) = screen.current.clone() {
                self.original = Some((screen.clone(), cur));
            }
        }
        // X11 and nothing close to the window's size: try one made to measure.
        let area = |w: u32, h: u32| u64::from(w) * u64::from(h);
        if screen.tool == Tool::Xrandr && area(mode.width, mode.height) * 100 < area(width, height) * 85 {
            match xrandr_custom(&screen.output, width, height) {
                Ok(name) => {
                    let custom = Mode { width: (width / 8) * 8, height, id: name.clone() };
                    match apply(&screen, &custom) {
                        Ok(()) => {
                            self.made.push((screen.output.clone(), name));
                            return Ok(true);
                        }
                        Err(e) => crate::tailscale::dbg(&format!("display: the made-to-measure mode failed: {e}")),
                    }
                    xrandr_forget(&screen.output, &name);
                }
                Err(e) => crate::tailscale::dbg(&format!("display: no made-to-measure mode: {e}")),
            }
        }
        if screen.current.as_ref().is_some_and(|c| c.width == mode.width && c.height == mode.height) {
            return Ok(false);
        }
        apply(&screen, &mode)?;
        Ok(true)
    }

    /// Back to how it was. True when it changed.
    pub fn restore(&mut self) -> bool {
        let Some((screen, mode)) = self.original.take() else { return false };
        let ok = apply(&screen, &mode).map_err(|e| crate::tailscale::dbg(&format!("display: can't restore the resolution: {e}"))).is_ok();
        for (output, name) in self.made.drain(..) {
            xrandr_forget(&output, &name);
        }
        ok
    }
}

impl Drop for Fitter {
    fn drop(&mut self) {
        self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_tool_says_why() {
        assert_eq!(run(Command::new("sh").args(["-c", "echo 1920x1080"])).as_deref(), Ok("1920x1080\n"));
        let failed = run(Command::new("sh").args(["-c", "echo no such mode >&2; exit 3"])).unwrap_err();
        assert!(failed.starts_with("sh: no such mode"), "{failed}");
        assert!(run(&mut Command::new("biglace-no-such-tool")).is_err());
    }

    const XRANDR: &str = "Screen 0: minimum 320 x 200, current 1280 x 800, maximum 8192 x 8192
Virtual-1 connected primary 1280x800+0+0 0mm x 0mm
   1280x800      74.99*+  59.81
   1920x1080     60.00    50.00
   1024x768      60.00
Virtual-2 disconnected (normal left inverted right x axis y axis)
";

    #[test]
    fn reads_xrandr_and_picks_the_mode_that_fits() {
        let s = parse_xrandr(XRANDR).unwrap();
        assert_eq!(s.output, "Virtual-1");
        assert_eq!(s.modes.len(), 3);
        assert_eq!(s.current.as_ref().map(|m| m.width), Some(1280));
        // A 1600×900 window: 1280×800 is the largest that fits.
        assert_eq!(pick(&s.modes, 1600, 900).map(|m| m.id.as_str()), Some("1280x800"));
        assert_eq!(pick(&s.modes, 2000, 1200).map(|m| m.id.as_str()), Some("1920x1080"));
        // Smaller than everything: the smallest.
        assert_eq!(pick(&s.modes, 500, 400).map(|m| m.id.as_str()), Some("1024x768"));
        // Two active screens: hands off.
        let two = XRANDR.replace("Virtual-2 disconnected (normal", "Virtual-2 connected 1024x768+1280+0 (normal");
        assert!(parse_xrandr(&two).is_none());
    }

    #[test]
    fn reads_kscreen() {
        let json = r#"{"outputs":[{"name":"Virtual-1","enabled":true,"currentModeId":"1",
            "modes":[{"id":"1","size":{"width":1280,"height":800}},{"id":"13","size":{"width":1920,"height":1080}}]},
            {"name":"Virtual-2","enabled":false,"modes":[]}]}"#;
        let s = parse_kscreen(json).unwrap();
        assert_eq!(s.output, "Virtual-1");
        assert_eq!(s.current.map(|m| m.id), Some("1".into()));
        assert_eq!(pick(&s.modes, 1920, 1080).map(|m| m.id.as_str()), Some("13"));
    }

    #[test]
    fn reads_gdctl() {
        let text = "Monitors:
└──Monitor Virtual-1 (Red Hat, Inc. 15\")
   ├──Vendor: RHT
   ├──Current mode
   │   └──1280x800@74.994
   └──Modes (3)
       ├──1920x1080@60.000
       ├──1280x800@74.994
       └──1024x768@60.004

Logical monitors:
└──Logical monitor #1
   ├──Position: (0, 0)
   ├──Scale: 1.0
   └──Monitors: (1)
       └──Virtual-1 (Red Hat, Inc. 15\")
";
        let s = parse_gdctl(text).unwrap();
        assert_eq!(s.output, "Virtual-1");
        assert_eq!(s.modes.len(), 3);
        assert_eq!(s.current.as_ref().map(|m| m.id.as_str()), Some("1280x800@74.994"));
        assert_eq!(s.scale.as_deref(), Some("1.0"));
        assert_eq!(pick(&s.modes, 1900, 1100).map(|m| m.id.as_str()), Some("1280x800@74.994"));
    }
}
