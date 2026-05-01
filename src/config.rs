use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server_url:   String,
    pub authkey:      String,
    pub hostname:     String,
    pub auto_connect: bool,

    #[serde(default)]
    pub panel_url:    String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_url:   String::new(),
            authkey:      String::new(),
            hostname:     default_hostname(),
            auto_connect: false,
            panel_url:    String::new(),
        }
    }
}

fn default_hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| {
            fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|_| "meu-pc".into())
}

fn path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/biglace/config.toml")
}

pub fn load() -> Config {
    let p = path();
    fs::read_to_string(&p)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(cfg: &Config) -> Result<()> {
    let p = path();
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&p, toml::to_string(cfg)?)?;
    Ok(())
}
