use crate::api::{GROUP, Request, group_id, send};
use crate::bucket::Bucket;
use crate::config::Config;
use crate::service::{self, BINARY_PATH, SERVICE_UNIT};
use crate::systemd;
use anyhow::{Context, Result, bail, ensure};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, Password};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

/// `mica service setup`: asks for the settings, checks them, and installs the
/// service. With `use_config_file`, it asks nothing and takes the file as it is.
pub async fn run(config_path: &Path, use_config_file: bool) -> Result<()> {
    ensure!(unsafe { libc::geteuid() } == 0, "run `mica service setup` with sudo");
    ensure!(systemd::is_available(), "systemd is not running on this machine");
    println!("Checking this machine");
    load_ublk_module()?;
    check_filesystem_tools();
    ensure_group()?;
    let config = if use_config_file { Config::read_complete_file(config_path)? } else { ask_config(config_path)? };
    prepare_data_dir(&config.data_dir)?;
    test_bucket(&config).await?;
    config.save(config_path)?;
    println!("  ok  saved {} (readable by root only)", config_path.display());
    install_binary()?;
    service::install(config_path, &config)?;
    start_service(&config).await?;
    offer_group_membership(!use_config_file)
}

/// Members of the mica group can use mica without sudo. The daemon makes its
/// socket readable and writable by this group.
fn ensure_group() -> Result<()> {
    if group_id(GROUP).is_some() {
        return Ok(());
    }
    let status = std::process::Command::new("groupadd").args(["--system", GROUP]).status()?;
    ensure!(status.success(), "cannot create the {GROUP} group");
    println!("  ok  created the {GROUP} group");
    Ok(())
}

/// Offers to add the user who ran sudo to the mica group.
fn offer_group_membership(interactive: bool) -> Result<()> {
    let Ok(user) = std::env::var("SUDO_USER") else { return Ok(()) };
    if user.is_empty() || user == "root" || is_group_member(&user) {
        return Ok(());
    }
    let question = format!("Add {user} to the {GROUP} group, to use mica without sudo?");
    if !interactive || !Confirm::with_theme(&ColorfulTheme::default()).with_prompt(question).default(true).interact()? {
        println!("To use mica without sudo: sudo usermod -aG {GROUP} {user}");
        return Ok(());
    }
    let status = std::process::Command::new("usermod").args(["-aG", GROUP, &user]).status()?;
    ensure!(status.success(), "cannot add {user} to the {GROUP} group");
    println!("Added {user} to the {GROUP} group. Log in again (or run `newgrp {GROUP}`) to use it");
    Ok(())
}

fn is_group_member(user: &str) -> bool {
    std::process::Command::new("id")
        .args(["-nG", user])
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).split_whitespace().any(|group| group == GROUP))
}

/// Loads the module now and at every boot.
fn load_ublk_module() -> Result<()> {
    let status = std::process::Command::new("modprobe").arg("ublk_drv").status().context("run modprobe")?;
    ensure!(status.success(), "cannot load the ublk_drv kernel module (needs Linux 6.0 or later)");
    std::fs::write("/etc/modules-load.d/mica.conf", "ublk_drv\n")?;
    println!("  ok  ublk_drv is loaded, and loads at boot");
    Ok(())
}

fn check_filesystem_tools() {
    for tool in ["mkfs.ext4", "mkfs.xfs"] {
        let found = std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .any(|dir| Path::new(dir).join(tool).exists());
        if !found {
            println!("  warn  {tool} is not installed; `mica disk mount --fs` cannot use it");
        }
    }
}

/// Each prompt shows the current value. Enter keeps it.
fn ask_config(config_path: &Path) -> Result<Config> {
    let mut config = Config::read_file(config_path)?.unwrap_or_default();
    if config.s3.region.is_empty() {
        config.s3.region = "auto".to_string();
    }
    println!("Press Enter to keep a value.");
    let s3 = &mut config.s3;
    s3.endpoint = ask("S3 endpoint", &s3.endpoint, false)?;
    s3.bucket = ask("Bucket", &s3.bucket, false)?;
    s3.access_key_id = ask("Access key ID", &s3.access_key_id, false)?;
    s3.secret_access_key = ask_secret(&s3.secret_access_key)?;
    s3.region = ask("Region", &s3.region, false)?;
    s3.prefix = ask("Key prefix (optional)", &s3.prefix, true)?;
    let data_dir = config.data_dir.display().to_string();
    let data_dir_prompt = format!("Data folder ({})", free_space_text(&config.data_dir));
    config.data_dir = ask(&data_dir_prompt, &data_dir, false)?.into();
    config.cache_limit_gib = ask("Cache limit in GiB", &config.cache_limit_gib.to_string(), false)?
        .parse()
        .context("the cache limit must be a number")?;
    Ok(config)
}

fn ask(prompt: &str, current: &str, optional: bool) -> Result<String> {
    let theme = ColorfulTheme::default();
    let mut input = Input::<String>::with_theme(&theme).with_prompt(prompt).allow_empty(optional);
    if !current.is_empty() {
        input = input.default(current.to_string());
    }
    Ok(input.interact_text()?.trim().to_string())
}

/// The secret is never shown. Empty input keeps the current one.
fn ask_secret(current: &str) -> Result<String> {
    let theme = ColorfulTheme::default();
    loop {
        let prompt = match current.len() {
            0 => "Secret access key".to_string(),
            len => format!("Secret access key [****{}]", &current[len.saturating_sub(4)..]),
        };
        let value = Password::with_theme(&theme).with_prompt(prompt).allow_empty_password(true).interact()?;
        match (value.is_empty(), current.is_empty()) {
            (false, _) => return Ok(value),
            (true, false) => return Ok(current.to_string()),
            (true, true) => println!("A secret access key is needed."),
        }
    }
}

/// Cache and dirty chunks must share one filesystem: a checkpoint renames
/// files from one to the other. Reflinks make chunk copies free.
fn prepare_data_dir(data_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(data_dir).with_context(|| format!("create {}", data_dir.display()))?;
    if supports_reflink(data_dir) {
        println!("  ok  {} supports reflink copies", data_dir.display());
    } else {
        println!("  warn  {} has no reflink copies (use XFS or btrfs); chunk copies cost real I/O", data_dir.display());
    }
    Ok(())
}

fn supports_reflink(dir: &Path) -> bool {
    let source_path = dir.join(".reflink-check-a");
    let target_path = dir.join(".reflink-check-b");
    let result = (|| -> std::io::Result<bool> {
        std::fs::write(&source_path, [0u8; 4096])?;
        let source = std::fs::File::open(&source_path)?;
        let target = std::fs::File::create(&target_path)?;
        Ok(unsafe { libc::ioctl(target.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } == 0)
    })();
    let _ = std::fs::remove_file(&source_path);
    let _ = std::fs::remove_file(&target_path);
    result.unwrap_or(false)
}

/// Writes, reads and deletes one small object, and names the step that fails.
async fn test_bucket(config: &Config) -> Result<()> {
    let bucket = Bucket::connect(&config.s3)?;
    let key = format!(".mica-setup-check/{}", std::fs::read_to_string("/proc/sys/kernel/random/uuid")?.trim());
    let data = bytes::Bytes::from_static(b"mica setup check");
    bucket.put(&key, data.clone()).await.context("bucket test failed at put")?;
    let read = bucket.get(&key).await.context("bucket test failed at get")?;
    ensure!(read.as_ref() == Some(&data), "bucket test failed: get returned other data");
    bucket.delete(&key).await.context("bucket test failed at delete")?;
    println!("  ok  the bucket accepts put, get and delete");
    Ok(())
}

/// The service must not run a binary from a build folder, so setup copies itself.
fn install_binary() -> Result<()> {
    let current = std::env::current_exe()?.canonicalize()?;
    let target = Path::new(BINARY_PATH);
    if target.canonicalize().ok().as_deref() == Some(current.as_path()) {
        return Ok(());
    }
    let temp = target.with_extension("new");
    std::fs::copy(&current, &temp).with_context(|| format!("copy mica to {}", temp.display()))?;
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))?;
    // A rename is safe while the old binary runs: the process keeps the old file.
    std::fs::rename(&temp, target)?;
    println!("  ok  installed {BINARY_PATH}");
    Ok(())
}

/// A running service restarts, so it picks up the new binary and config.
/// Its devices pause for a moment and continue.
async fn start_service(config: &Config) -> Result<()> {
    if systemd::systemctl_check(&["is-active", "--quiet", SERVICE_UNIT]) {
        service::refuse_if_not_recoverable(config).await?;
        systemd::systemctl(&["enable", SERVICE_UNIT])?;
        systemd::systemctl(&["restart", SERVICE_UNIT])?;
    } else {
        systemd::systemctl(&["enable", "--now", SERVICE_UNIT])?;
    }
    for _ in 0..20 {
        if let Ok(status) = send(&config.socket, &Request::Status).await {
            let (node, node_id) = (status["node"].as_str().unwrap_or("?"), status["node_id"].as_str().unwrap_or("?"));
            println!("  ok  mica is running on {node} (node id {node_id})");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("mica.service started but does not answer. See `mica service logs`")
}

fn free_space_text(dir: &Path) -> String {
    let existing = dir.ancestors().find(|path| path.exists()).unwrap_or(Path::new("/"));
    let Ok(path) = std::ffi::CString::new(existing.as_os_str().as_encoded_bytes()) else {
        return "free space unknown".to_string();
    };
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
        return "free space unknown".to_string();
    }
    format!("{} GiB free", (stats.f_bavail as u64 * stats.f_frsize as u64) >> 30)
}
