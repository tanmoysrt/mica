//! The settings of the node, in `/etc/mica/config.toml`: three paths.
//!
//! The file is optional: each path has a default. The settings of a disk
//! are not here. They are in the head of the disk, so they follow the disk
//! between nodes (see `store::Limits`). The bucket of a disk comes with each
//! command (see `store::StoreConfig`).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_PATH: &str = "/etc/mica/config.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The socket of the daemon. Root only.
    pub socket: PathBuf,
    /// Data files, dirty logs and the cache.
    pub data_dir: PathBuf,
    /// Files that must not survive a reboot: credentials and recent bitmaps.
    pub run_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            socket: "/run/mica.sock".into(),
            data_dir: "/var/lib/mica".into(),
            run_dir: "/run/mica".into(),
        }
    }
}

impl Config {
    /// A missing file at the default path gives the defaults.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && path == Path::new(DEFAULT_PATH) => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

}

/// Plain bytes, or a number with K, M, G or T (powers of 1024).
pub fn parse_size(text: &str) -> Result<u64> {
    let text = text.trim();
    let (number, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len()));
    let shift = match unit.trim().to_ascii_uppercase().trim_end_matches(['B', 'I']) {
        "" => 0,
        "K" => 10,
        "M" => 20,
        "G" => 30,
        "T" => 40,
        _ => bail!("bad size {text:?}: use a number with K, M, G or T"),
    };
    let number: u64 = number.parse().with_context(|| format!("bad size {text:?}"))?;
    number.checked_mul(1 << shift).with_context(|| format!("size {text:?} is too large"))
}

/// Seconds, or a number with s, m or h.
pub fn parse_duration(text: &str) -> Result<u64> {
    let text = text.trim();
    let (number, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len()));
    let scale = match unit.trim() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => bail!("bad interval {text:?}: use a number with s, m or h"),
    };
    let number: u64 = number.parse().with_context(|| format!("bad interval {text:?}"))?;
    Ok(number * scale)
}

pub fn format_duration(seconds: u64) -> String {
    match seconds {
        _ if seconds.is_multiple_of(3600) => format!("{}h", seconds / 3600),
        _ if seconds.is_multiple_of(60) => format!("{}m", seconds / 60),
        _ => format!("{seconds}s"),
    }
}

/// A size in the units of `parse_size`, such as 512M or 1.5G. One decimal below 10 only.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => bytes.to_string(),
        _ if value.fract() == 0.0 || value >= 10.0 => format!("{value:.0}{}", UNITS[unit]),
        _ => format!("{value:.1}{}", UNITS[unit]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_and_print() {
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("20GiB").unwrap(), 20 << 30);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("1X").is_err());
        assert_eq!(format_size(3 << 29), "1.5G");
        assert_eq!(parse_size(&format_size(512 << 20)).unwrap(), 512 << 20);
        assert_eq!(parse_duration("5m").unwrap(), 300);
        assert_eq!(parse_duration("90").unwrap(), 90);
        assert!(parse_duration("5d").is_err());
        assert_eq!(format_duration(300), "5m");
    }
}
