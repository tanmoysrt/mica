//! Progress on the terminal while the daemon works: a spinner for a short
//! wait, and a bar for an upload or a copy, with the time left. The bar
//! reads the status of the daemon on a second connection. Without a
//! terminal, nothing shows.

use crate::config::format_size;
use crate::daemon::api::{Request, call};
use anyhow::Result;
use indicatif::{ProgressBar, ProgressState, ProgressStyle};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

const TICK: Duration = Duration::from_millis(100);
const POLL: Duration = Duration::from_millis(500);
/// Bytes up before the speed of an upload means much.
const SETTLED: u64 = 8 << 20;

pub async fn call_with_spinner(socket: &Path, request: &Request, message: String) -> Result<Value> {
    let spinner = ProgressBar::new_spinner().with_style(ProgressStyle::with_template("{spinner:.cyan} {msg}").unwrap()).with_message(message);
    spinner.enable_steady_tick(TICK);
    let result = call(socket, request).await;
    spinner.finish_and_clear();
    result
}

/// Sends a request that uploads the unsaved data of `disk`, and shows the
/// upload. The data left is the unsaved data that no running transaction
/// has uploaded yet. A transaction that runs at the start already counts.
pub async fn call_with_upload(socket: &Path, request: &Request, disk: &str, message: String) -> Result<Value> {
    let (mut total, mut done) = (0, 0);
    let sample = move |status: &Value| {
        let found = status["disks"].as_array()?.iter().find(|found| found["disk"] == disk)?;
        let left = found["status"]["unsaved_bytes"].as_u64()?.saturating_sub(found["status"]["staged_bytes"].as_u64()?);
        // Grows when a later transaction takes chunks that the first one left out.
        total = total.max(done + left);
        // Two locks in the daemon: a sample can lag one chunk behind the last.
        done = done.max(total - left);
        (total > 0).then_some((done, total))
    };
    follow(socket, request, message, sample, upload_style).await
}

/// Sends a request that runs as a task of the daemon for `disk`, such as
/// the copy of a restore, and shows its count.
pub async fn call_with_task(socket: &Path, request: &Request, disk: &str, message: String) -> Result<Value> {
    let sample = |status: &Value| {
        let task = status["tasks"].as_array()?.iter().find(|task| task["disk"] == disk)?;
        let total = task["total"].as_u64().filter(|&total| total > 0)?;
        Some((task["done"].as_u64()?, total))
    };
    follow(socket, request, message, sample, count_style).await
}

/// Shows a spinner until `sample` gives a position and a length from the
/// status of the daemon, then a bar in `style`.
async fn follow(
    socket: &Path,
    request: &Request,
    message: String,
    mut sample: impl FnMut(&Value) -> Option<(u64, u64)>,
    style: fn() -> ProgressStyle,
) -> Result<Value> {
    let bar = ProgressBar::new(0).with_style(ProgressStyle::with_template("{spinner:.cyan} {msg}").unwrap()).with_message(message);
    bar.enable_steady_tick(TICK);
    let answer = call(socket, request);
    tokio::pin!(answer);
    let result = loop {
        tokio::select! {
            result = &mut answer => break result,
            _ = tokio::time::sleep(POLL) => {}
        }
        let Ok(status) = call(socket, &Request::default()).await else { continue };
        let Some((position, length)) = sample(&status) else { continue };
        if bar.length() == Some(0) {
            bar.set_style(style());
        }
        bar.set_length(length);
        bar.set_position(position);
    };
    bar.finish_and_clear();
    result
}

/// Such as `⠙ Uploading data  ━━━━━━━──────── 213M / 1000M  10.1M/s  1m 05s left`.
fn upload_style() -> ProgressStyle {
    bar_style("{spinner:.cyan} {msg}  {bar:24.cyan/dim} {sizes}  {speed}  {left}")
        .with_key("sizes", |state: &ProgressState, out: &mut dyn std::fmt::Write| {
            let _ = write!(out, "{} / {}", format_size(state.pos()), format_size(state.len().unwrap_or(0)));
        })
        .with_key("speed", |state: &ProgressState, out: &mut dyn std::fmt::Write| {
            let _ = write!(out, "{}/s", format_size(state.per_sec() as u64));
        })
        .with_key("left", |state: &ProgressState, out: &mut dyn std::fmt::Write| {
            let _ = match state.pos() < SETTLED {
                true => write!(out, "starting"),
                false => write!(out, "{} left", clock(state.eta().as_secs())),
            };
        })
}

/// Such as `⠙ Copying into disk db-old  ━━━━━━━──────── 1200 / 3100 objects  20s left`.
fn count_style() -> ProgressStyle {
    bar_style("{spinner:.cyan} {msg}  {bar:24.cyan/dim} {pos} / {len} objects  {left}").with_key(
        "left",
        |state: &ProgressState, out: &mut dyn std::fmt::Write| {
            let _ = write!(out, "{} left", clock(state.eta().as_secs()));
        },
    )
}

fn bar_style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template).unwrap().progress_chars("━━─")
}

fn clock(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}
