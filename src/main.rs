mod api;
mod blocking;
mod bucket;
mod catalog;
mod checkpoint;
mod chunk_cache;
mod commands;
mod config;
mod content_hash;
mod daemon;
mod daemon_recovery;
mod dirty_folder;
mod disk_commands;
mod gc;
mod disk;
mod local_io;
mod local_state;
mod managed_mount;
mod manifest;
mod mount_tools;
mod node;
mod output;
mod ownership;
mod queue_io;
mod read_profile;
mod records;
mod service;
mod setup;
mod snapshot_commands;
mod systemd;
mod ublk_device;

use anyhow::{Result, bail};
use clap::Parser;
use commands::Command;
use config::{Config, DEFAULT_CONFIG_PATH};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "mica", about = "Disks that live in S3")]
struct Cli {
    #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    match cli.command {
        Command::Daemon => {
            // Must happen before tokio starts its threads: unshare fails in a threaded process.
            isolate_mount_namespace()?;
            raise_open_file_limit();
            limit_allocator_caching();
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            runtime.block_on(run_daemon(config, cli.config))
        }
        command => {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            runtime.block_on(commands::run(command, &cli.config, &config))
        }
    }
}

async fn run_daemon(config: Config, config_path: PathBuf) -> Result<()> {
    let daemon = daemon::Daemon::start(config, config_path).await?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = api::serve(daemon.clone()) => result,
        _ = terminate.recv() => shut_down(&daemon).await,
        _ = tokio::signal::ctrl_c() => shut_down(&daemon).await,
    }
}

/// Exits without running destructors: dropping a device control would delete
/// the device that the next daemon is meant to take over.
async fn shut_down(daemon: &daemon::Daemon) -> Result<()> {
    log::info!("shutting down");
    daemon.shut_down().await;
    std::process::exit(0)
}

/// By default glibc raises its threshold for mmap-backed allocations after
/// the first large free. Then the 4 MiB upload buffers come from the heap
/// and are never returned to the system. A fixed threshold, and fewer arenas
/// for the many worker threads, let memory go back after an upload.
fn limit_allocator_caching() {
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 256 << 10);
        libc::mallopt(libc::M_ARENA_MAX, 4);
    }
}

/// mica keeps one open file per dirty chunk. The default limit of 1024 is
/// too low for a large write burst.
fn raise_open_file_limit() {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 && limit.rlim_cur < limit.rlim_max {
            limit.rlim_cur = limit.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
        }
    }
}

/// The ublk daemon must not share a mount namespace with users of its own
/// devices. If it did, a mount of /dev/ublkbN in its namespace could flush
/// through the daemon while it exits, and deadlock. See the libublk README.
///
/// Slave, not private: host unmounts must reach this namespace too. With a
/// private namespace, a mount copied at start keeps the device busy forever.
fn isolate_mount_namespace() -> Result<()> {
    unsafe {
        if libc::unshare(libc::CLONE_NEWNS) != 0 {
            bail!("unshare mount namespace: {}", std::io::Error::last_os_error());
        }
        let root = c"/";
        let flags = libc::MS_REC | libc::MS_SLAVE;
        if libc::mount(std::ptr::null(), root.as_ptr(), std::ptr::null(), flags, std::ptr::null()) != 0 {
            bail!("make mounts slave: {}", std::io::Error::last_os_error());
        }
    }
    Ok(())
}
