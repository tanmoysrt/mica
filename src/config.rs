//! The settings of the node, in `/etc/mica/config.toml`.
//!
//! The file is optional: each setting has a default. The settings of the
//! bucket are not here. Each disk has its own bucket, and its credentials
//! come with each command (see `store::StoreConfig`).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    pub transaction_interval_seconds: u64,
    /// A transaction starts early when this much unsaved data waits.
    pub transaction_after: String,
    /// The budgets of a new disk, when `mica disk create` gives none.
    pub cache_size: String,
    pub unsaved_size: String,
    /// A disk is behind when its oldest unsaved write is older than this.
    pub max_unsaved_minutes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            socket: "/run/mica.sock".into(),
            data_dir: "/var/lib/mica".into(),
            run_dir: "/run/mica".into(),
            transaction_interval_seconds: 30,
            transaction_after: "512M".into(),
            cache_size: "512M".into(),
            unsaved_size: "4G".into(),
            max_unsaved_minutes: 5,
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
        let config: Self = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        for size in [&config.transaction_after, &config.cache_size, &config.unsaved_size] {
            parse_size(size)?;
        }
        Ok(config)
    }

    pub fn tuning(&self) -> crate::disk::Tuning {
        crate::disk::Tuning {
            transaction_interval: Duration::from_secs(self.transaction_interval_seconds.max(1)),
            transaction_after: parse_size(&self.transaction_after).unwrap_or(512 << 20),
            max_unsaved_age: Duration::from_secs(self.max_unsaved_minutes * 60),
            keep_saved_local: 64 << 20,
        }
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

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes}B"),
        _ if value.fract() == 0.0 => format!("{value:.0}{}", UNITS[unit]),
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
        assert_eq!(format_size(3 << 29), "1.5GiB");
    }
}
