//! Who may use this computer remotely, and what happened.
//!
//! - Rules per person (account): ask each time (the default), always
//!   allow, or never. "Always" comes from ticking "remember" when allowing,
//!   or from Preferences; "never" refuses without bothering the person here.
//! - A history of every access (who, from which device, what, how it went),
//!   kept on this computer (`access-log.jsonl`, the latest 500).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Rule {
    #[default]
    Ask,
    Always,
    Never,
}

/// One line of the history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Unix seconds.
    pub at: u64,
    pub who: String,
    #[serde(default)]
    pub device: String,
    /// `screen`, `restart`, `relogin`, `shutdown`, `watch`, `control`,
    /// `terminal`, `files`.
    pub what: String,
    /// `allowed`, `remembered`, `denied`, `blocked`, `returning`, `ended`.
    pub outcome: String,
}

const KEEP: usize = 500;

fn dir() -> PathBuf {
    gtk4::glib::user_config_dir().join("biglace")
}

fn rules_path() -> PathBuf {
    dir().join("access-rules.json")
}

fn log_path() -> PathBuf {
    gtk4::glib::user_data_dir().join("biglace").join("access-log.jsonl")
}

fn rules_cache() -> &'static Mutex<BTreeMap<String, Rule>> {
    static R: OnceLock<Mutex<BTreeMap<String, Rule>>> = OnceLock::new();
    R.get_or_init(|| {
        let map = std::fs::read(rules_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Mutex::new(map)
    })
}

/// The rule for `who` (ask, when none was set).
pub fn rule(who: &str) -> Rule {
    rules_cache().lock().ok().and_then(|r| r.get(who).copied()).unwrap_or_default()
}

/// Every person with a rule other than "ask".
pub fn rules() -> BTreeMap<String, Rule> {
    rules_cache().lock().map(|r| r.clone()).unwrap_or_default()
}

pub fn set_rule(who: &str, rule: Rule) {
    let Ok(mut r) = rules_cache().lock() else { return };
    if rule == Rule::Ask {
        r.remove(who);
    } else {
        r.insert(who.to_string(), rule);
    }
    save_rules(&r);
}

/// The account `from` is called `to` now (the panel keeps the old name for it
/// alone): its rule goes along.
pub fn person_renamed(from: &str, to: &str) {
    let Ok(mut r) = rules_cache().lock() else { return };
    let Some(rule) = r.remove(from) else { return };
    r.entry(to.to_string()).or_insert(rule);
    save_rules(&r);
}

fn save_rules(r: &BTreeMap<String, Rule>) {
    if let Ok(json) = serde_json::to_vec_pretty(r) {
        let _ = std::fs::create_dir_all(dir());
        write_private(&rules_path(), &json);
    }
}

fn write_private(path: &std::path::Path, data: &[u8]) {
    use std::os::unix::fs::OpenOptionsExt;
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path) {
        let _ = f.write_all(data);
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Note an access in the history.
pub fn log(who: &str, device: &str, what: &str, outcome: &str) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock();
    let entry = Entry { at: now(), who: who.into(), device: device.into(), what: what.into(), outcome: outcome.into() };
    let Ok(line) = serde_json::to_string(&entry) else { return };
    let path = log_path();
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default();
    lines.push(line);
    if lines.len() > KEEP {
        lines.drain(..lines.len() - KEEP);
    }
    let mut text = lines.join("\n");
    text.push('\n');
    write_private(&path, text.as_bytes());
}

/// The history, newest first.
pub fn history() -> Vec<Entry> {
    let mut out: Vec<Entry> = std::fs::read_to_string(log_path())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    out.reverse();
    out
}

pub fn clear_history() {
    let _ = std::fs::remove_file(log_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_read_back_and_default_to_ask() {
        assert_eq!(Rule::default(), Rule::Ask);
        let json = serde_json::to_string(&BTreeMap::from([("bruna".to_string(), Rule::Always)])).unwrap();
        assert_eq!(json, r#"{"bruna":"always"}"#);
        let back: BTreeMap<String, Rule> = serde_json::from_str(r#"{"x":"never"}"#).unwrap();
        assert_eq!(back["x"], Rule::Never);
    }
}
