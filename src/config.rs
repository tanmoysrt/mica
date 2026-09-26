use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/mica/config.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub socket: PathBuf,
    pub data_dir: PathBuf,
    pub cache_limit_gib: u64,
    pub dirty_limit_gib: u64,
    pub checkpoint_interval_secs: u64,
    /// `mica disk mount` only mounts below these folders.
    pub mount_roots: Vec<PathBuf>,
    /// GC keeps this many checkpoints of each disk as restore points.
    pub gc_keep_checkpoints: usize,
    /// GC never deletes an object written in the last hours. A running
    /// checkpoint may not have committed its new chunks yet.
    pub gc_grace_hours: u64,
    /// `MemoryHigh=` of mica.service, such as "2G". Empty means no limit:
    /// the kernel frees mica's page cache anyway when other programs need memory.
    pub memory_high: String,
    pub s3: S3Config,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub region: String,
    /// Optional key prefix, so that one bucket can hold many mica setups.
    pub prefix: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            socket: PathBuf::from("/run/mica.sock"),
            data_dir: PathBuf::from("/var/lib/mica"),
            cache_limit_gib: 50,
            dirty_limit_gib: 20,
            checkpoint_interval_secs: 180,
            mount_roots: ["/mnt", "/srv", "/media"].into_iter().map(PathBuf::from).collect(),
            gc_keep_checkpoints: 5,
            gc_grace_hours: 24,
            memory_high: String::new(),
            s3: S3Config { region: "auto".to_string(), ..S3Config::default() },
        }
    }
}

impl Config {
    /// Loads the config file. A missing file gives the defaults.
    /// Empty S3 fields are filled from the environment (same names as `.env`),
    /// which helps when the daemon runs by hand during development.
    ///
    /// A user in the mica group cannot read the file (it holds the secret key).
    /// The CLI then uses the defaults: it only needs the socket path.
    pub fn load(path: &Path) -> Result<Self> {
        let file = match std::fs::metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => None,
            _ => match std::fs::File::open(path) {
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => None,
                _ => Self::read_file(path)?,
            },
        };
        let mut config = file.unwrap_or_default();
        config.s3.fill_from_env();
        Ok(config)
    }

    /// Reads only the file, never the environment. A systemd service does not
    /// get the shell environment, so setup must judge the file alone.
    pub fn read_file(path: &Path) -> Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Reads the file and fails with every missing field listed at once.
    pub fn read_complete_file(path: &Path) -> Result<Self> {
        let Some(config) = Self::read_file(path)? else {
            bail!("{} does not exist", path.display());
        };
        let missing = config.s3.missing_fields();
        if !missing.is_empty() {
            let lines: Vec<String> = missing.iter().map(|field| format!("  - s3.{field} is not set")).collect();
            bail!("{} is incomplete:\n{}", path.display(), lines.join("\n"));
        }
        Ok(config)
    }

    /// Writes the file with mode 0600, because it holds the secret key.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension("toml.tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)
            .with_context(|| format!("write {}", temp.display()))?;
        file.write_all(toml::to_string_pretty(self)?.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        Ok(())
    }

    /// systemd takes a size with K, M, G or T, a percentage, or "infinity".
    pub fn validate_memory_high(&self) -> Result<()> {
        let value = self.memory_high.trim();
        let number = value.trim_end_matches(['K', 'M', 'G', 'T', '%']);
        let valid = value.is_empty()
            || value == "infinity"
            || (!number.is_empty() && number.len() + 1 >= value.len() && number.parse::<u64>().is_ok());
        if !valid {
            bail!("memory_high must be like 2G, 512M, 50% or infinity, not {value:?}");
        }
        Ok(())
    }

    pub fn cache_limit_bytes(&self) -> u64 {
        self.cache_limit_gib << 30
    }

    pub fn dirty_limit_bytes(&self) -> u64 {
        self.dirty_limit_gib << 30
    }
}

impl S3Config {
    pub fn validate(&self) -> Result<()> {
        if let Some(field) = self.missing_fields().first() {
            bail!("s3.{field} is not set in the config file or the environment");
        }
        Ok(())
    }

    fn missing_fields(&self) -> Vec<&'static str> {
        [
            ("endpoint", &self.endpoint),
            ("bucket", &self.bucket),
            ("access_key_id", &self.access_key_id),
            ("secret_access_key", &self.secret_access_key),
        ]
        .into_iter()
        .filter(|(_, value)| value.is_empty())
        .map(|(name, _)| name)
        .collect()
    }

    fn fill_from_env(&mut self) {
        fill(&mut self.endpoint, "S3_ENDPOINT");
        fill(&mut self.bucket, "BUCKET_NAME");
        fill(&mut self.access_key_id, "ACCESS_KEY_ID");
        fill(&mut self.secret_access_key, "SECRET_ACCESS_KEY");
        fill(&mut self.region, "S3_REGION");
        fill(&mut self.prefix, "S3_PREFIX");
        if self.region.is_empty() {
            self.region = "auto".to_string();
        }
    }
}

fn fill(field: &mut String, env_name: &str) {
    if field.is_empty()
        && let Ok(value) = std::env::var(env_name)
    {
        *field = value;
    }
}
