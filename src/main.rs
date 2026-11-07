//! mica gives a virtual machine, a container or a host a block device whose
//! data lives in an S3 bucket. The node keeps a cache and the newest writes
//! on its local disk. A disk can move between nodes: detach it on one node,
//! attach it on another, and the new node downloads only what it reads.
//!
//! The code has these parts:
//!
//! - `store`: the layout of a disk in its bucket, and the rules that delete
//!   the objects that no version needs.
//! - `owner`: the claim, which makes one node the owner of a disk.
//! - `local`: the files on the node: data files, dirty log, cache.
//! - `disk`: the engine of an attached disk: read, write, flush, transactions.
//! - `ublk`: the block device in the kernel.
//! - `daemon`: the service of the node, and its API.
//! - `cli`: the `mica` command.

mod cli;
mod config;
mod daemon;
mod disk;
mod local;
mod owner;
mod store;
mod transfers;
mod ublk;

use anyhow::{Result, bail};
use clap::Parser;
use cli::{Cli, Command};

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    let config = config::Config::load(&cli.config)?;
    if let Command::Daemon = cli.command {
        // Before tokio starts its threads: unshare fails in a process with threads.
        isolate_mount_namespace()?;
        raise_open_file_limit();
        return tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run_daemon(config));
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(cli::run(cli, config))
}

async fn run_daemon(config: config::Config) -> Result<()> {
    let daemon = daemon::Daemon::start(config.clone()).await?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = daemon::api::serve(daemon.clone(), &config.socket) => result?,
        _ = terminate.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    log::info!("stopping: the devices stay paused for the next start");
    daemon.shut_down().await;
    // No destructors: a dropped device control would delete the device
    // that the next daemon takes over.
    std::process::exit(0)
}

/// A daemon that shares the mount namespace of the users of its devices
/// can deadlock: a mount of its own device in its namespace can flush
/// through the daemon while it exits. Slave, so that host unmounts still
/// reach this namespace.
fn isolate_mount_namespace() -> Result<()> {
    unsafe {
        if libc::unshare(libc::CLONE_NEWNS) != 0 {
            bail!("unshare the mount namespace: {}", std::io::Error::last_os_error());
        }
        let flags = libc::MS_REC | libc::MS_SLAVE;
        if libc::mount(std::ptr::null(), c"/".as_ptr(), std::ptr::null(), flags, std::ptr::null()) != 0 {
            bail!("make the mounts slave: {}", std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// mica keeps one data file open for each GiB of each attached disk.
fn raise_open_file_limit() {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 {
            limit.rlim_cur = limit.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
        }
    }
}
