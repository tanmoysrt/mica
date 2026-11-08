//! Progress on the terminal while the daemon works: a spinner for a short
//! wait, and a bar for an upload, with speed and time left. The bar reads
//! the status of the disk on a second connection. Without a terminal,
//! nothing shows.

use crate::config::format_size;
use crate::daemon::api::{Command, Request, call};
use anyhow::Result;
use indicatif::{ProgressBar, ProgressState, ProgressStyle};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

const TICK: Duration = Duration::from_millis(100);
const POLL: Duration = Duration::from_millis(500);
const SETTLED: u64 = 8 << 20;

/// Sends the request, and shows a spinner with `message` until the answer.
pub async fn call_with_spinner(socket: &Path, request: &Request, message: String) -> Result<Value> {
    let spinner = ProgressBar::new_spinner().with_style(ProgressStyle::with_template("{spinner:.cyan} {msg}").unwrap()).with_message(message);
    spinner.enable_steady_tick(TICK);
    let result = call(socket, request).await;
    spinner.finish_and_clear();
    result
}

/// Sends a request that uploads the unsaved data of `disk`, and shows the upload.
pub async fn call_with_upload(socket: &Path, request: &Request, disk: &str, message: String) -> Result<Value> {
    let bar = ProgressBar::new(0).with_message(message);
    bar.set_style(ProgressStyle::with_template("{spinner:.cyan} {msg}").unwrap());
    bar.enable_steady_tick(TICK);
    let answer = call(socket, request);
    tokio::pin!(answer);
    let mut start: Option<(u64, u64)> = None;
    let result = loop {
        tokio::select! {
            result = &mut answer => break result,
            _ = tokio::time::sleep(POLL) => {}
        }
        let Some((unsaved, uploaded)) = upload_state(socket, disk).await else { continue };
        let (total, base) = *start.get_or_insert((unsaved, uploaded));
        if total == 0 {
            continue;
        }
        if bar.length() == Some(0) {
            bar.set_style(upload_style());
            bar.set_length(total);
        }
        bar.set_position(uploaded.saturating_sub(base).min(total));
    };
    bar.finish_and_clear();
    result
}

/// Such as `⠙ Uploading data  ━━━━━━━──────── 213M / 1000M  10.1M/s  1m left`.
fn upload_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.cyan} {msg}  {bar:24.cyan/dim} {sizes}  {speed}  {left}")
        .unwrap()
        .progress_chars("━━─")
        .with_key("sizes", |state: &ProgressState, out: &mut dyn std::fmt::Write| {
            let _ = write!(out, "{} / {}", format_size(state.pos()), format_size(state.len().unwrap_or(0)));
        })
        .with_key("speed", |state: &ProgressState, out: &mut dyn std::fmt::Write| {
            let _ = write!(out, "{}/s", format_size(state.per_sec() as u64));
        })
        .with_key("left", |state: &ProgressState, out: &mut dyn std::fmt::Write| {
            // The speed means little before the first chunks are up.
            let _ = match state.pos() < SETTLED {
                true => write!(out, "starting"),
                false => write!(out, "{} left", clock(state.eta().as_secs())),
            };
        })
}

/// The unsaved bytes of the disk, and the bytes that it uploaded since its attach.
async fn upload_state(socket: &Path, disk: &str) -> Option<(u64, u64)> {
    let status = call(socket, &Request { store: None, command: Command::Status }).await.ok()?;
    let found = status["disks"].as_array()?.iter().find(|found| found["disk"] == disk)?;
    Some((found["status"]["unsaved_bytes"].as_u64()?, found["status"]["requests"]["bytes_out"].as_u64()?))
}

/// Such as 45s, 2m 05s or 1h 10m.
fn clock(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}
