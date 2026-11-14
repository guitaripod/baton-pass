use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::PathBuf;

pub const DEFAULT_PORT: u16 = 47474;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub fn to_wire(self) -> u8 {
        match self {
            Side::Left => 0,
            Side::Right => 1,
            Side::Top => 2,
            Side::Bottom => 3,
        }
    }

    #[cfg(target_os = "macos")]
    pub fn from_wire(value: u8) -> Option<Self> {
        Some(match value {
            0 => Side::Left,
            1 => Side::Right,
            2 => Side::Top,
            3 => Side::Bottom,
            _ => return None,
        })
    }

    #[cfg(target_os = "macos")]
    pub fn opposite(self) -> Self {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }
}

/// Appends the default port unless the peer already names one; bare IPv6 addresses get brackets.
fn with_default_port(peer: &str) -> String {
    let colons = peer.matches(':').count();
    if peer.starts_with('[') {
        if peer.contains("]:") { peer.to_string() } else { format!("{peer}:{DEFAULT_PORT}") }
    } else if colons > 1 {
        format!("[{peer}]:{DEFAULT_PORT}")
    } else if colons == 1 {
        peer.to_string()
    } else {
        format!("{peer}:{DEFAULT_PORT}")
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Confine {
    Builtin,
    All,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub peers: Vec<String>,
    pub listen: String,
    pub side: Side,
    pub hotkey: String,
    pub speed: f32,
    pub natural_scroll: bool,
    pub swap_alt_meta: bool,
    pub clipboard: bool,
    pub confine: Confine,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            peers: Vec::new(),
            listen: format!("0.0.0.0:{DEFAULT_PORT}"),
            side: Side::Left,
            hotkey: "Meta+Escape".into(),
            speed: 1.0,
            natural_scroll: true,
            swap_alt_meta: false,
            clipboard: true,
            confine: Confine::Builtin,
        }
    }
}

impl Config {
    pub fn dir() -> PathBuf {
        #[cfg(target_os = "macos")]
        {
            PathBuf::from("/usr/local/etc/ferry")
        }
        #[cfg(not(target_os = "macos"))]
        {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
                .unwrap_or_else(|| PathBuf::from("."))
                .join("ferry")
        }
    }

    pub fn path() -> PathBuf {
        Self::dir().join("config.toml")
    }

    pub fn key_path() -> PathBuf {
        Self::dir().join("key")
    }

    pub fn load() -> Result<Self> {
        let path = Self::path();
        let config: Config = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        if !(0.05..=20.0).contains(&config.speed) {
            bail!("speed must be between 0.05 and 20, got {}", config.speed);
        }
        Ok(config)
    }

    pub fn peer_addresses(&self) -> Vec<String> {
        self.peers.iter().map(|peer| with_default_port(peer)).collect()
    }
}
