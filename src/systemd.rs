use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

pub const UNIT_DIR: &str = "/etc/systemd/system";

pub fn is_available() -> bool {
    Path::new("/run/systemd/system").exists()
}

/// Runs systemctl and fails when it fails. Its output goes to the terminal.
pub fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl").args(args).status().context("run systemctl")?;
    if !status.success() {
        bail!("systemctl {} failed: {status}", args.join(" "));
    }
    Ok(())
}

/// Runs systemctl for a yes/no answer, such as `is-active`.
pub fn systemctl_check(args: &[&str]) -> bool {
    Command::new("systemctl")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub fn unit_exists(name: &str) -> bool {
    Path::new(UNIT_DIR).join(name).exists()
}

pub fn write_unit(name: &str, content: &str) -> Result<()> {
    let path = Path::new(UNIT_DIR).join(name);
    std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))
}

pub fn remove_unit(name: &str) -> Result<()> {
    match std::fs::remove_file(Path::new(UNIT_DIR).join(name)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

/// Tells systemd that the daemon is ready (`Type=notify`). Does nothing
/// when the daemon does not run under systemd.
pub fn notify_ready() {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    let Ok(target) = std::env::var("NOTIFY_SOCKET") else { return };
    // A leading '@' means an abstract socket address.
    let address = match target.strip_prefix('@') {
        Some(name) => SocketAddr::from_abstract_name(name.as_bytes()),
        None => SocketAddr::from_pathname(&target),
    };
    let sent = address.and_then(|address| UnixDatagram::unbound()?.send_to_addr(b"READY=1", &address));
    if let Err(error) = sent {
        log::warn!("cannot notify systemd: {error}");
    }
}
