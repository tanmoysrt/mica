//! Human output of the CLI. `--json` prints the answer of the daemon instead.
//!
//! Each answer says what happened, and then the next step as a command to
//! copy. Colors show only on a terminal.

use crate::config::{format_duration, format_size};
use crate::daemon::api::Command;
use crate::owner::unix_now;
use console::{Alignment, measure_text_width, pad_str, style};
use serde_json::Value;

pub fn print(command: &Command, result: &Value) {
    match command {
        Command::Status => status(result),
        Command::List => list(result),
        Command::Info { .. } => info(result),
        Command::Create { disk, .. } => {
            done(&format!("Made disk {disk}: {} in chunks of {}", size(&result["size"]), size(&result["chunk_size"])));
            warn(&result["warning"]);
            next("Attach it", &format!("sudo mica disk attach {disk}"));
        }
        Command::Attach { disk } => {
            let device = text(&result["device"]);
            done(&format!("Attached disk {disk} as {device}"));
            if result["blank"].as_bool() == Some(true) {
                next("The disk is new. Make a filesystem on it once", &format!("sudo mkfs.ext4 {device}"));
            }
            next("Mount it", &format!("sudo mkdir -p /mnt/{disk} && sudo mount -o discard {device} /mnt/{disk}"));
        }
        Command::Detach { disk } => done(&format!("Detached disk {disk}. The bucket has all its data, so any machine can attach it")),
        Command::Set { disk, size: new_size, .. } => {
            let limits = &result["limits"];
            done(&format!(
                "Changed disk {disk}: size {}, cache {}, unsaved up to {}, saves every {}",
                size(&result["size"]),
                size(&limits["cache_bytes"]),
                size(&limits["unsaved_bytes"]),
                interval(&limits["interval_seconds"])
            ));
            if new_size.is_some() {
                next("After the next attach, grow its filesystem", &format!("sudo resize2fs /dev/mica/{disk}"));
            }
        }
        Command::Release { disk } => {
            done(&format!("Released disk {disk} from {}. Any machine can attach it now", text(&result["released_from"])));
            eprintln!("  Its changes that were not in the bucket are lost.");
        }
        Command::Delete { disk } => done(&format!("Deleted disk {disk} and its snapshots")),
        Command::SnapshotCreate { disk, snapshot } => done(&format!("Took snapshot {snapshot} of disk {disk}")),
        Command::SnapshotDelete { disk, snapshot } => done(&format!("Deleted snapshot {snapshot} of disk {disk}")),
        Command::BucketAdd => {
            let disks = result["disks"].as_u64().unwrap_or(0);
            done(&format!("Added bucket {}. It holds {disks} {}", text(&result["bucket"]), if disks == 1 { "disk" } else { "disks" }));
            warn(&result["warning"]);
            next("Make a disk in it", "sudo mica disk create <disk> --size 50G");
        }
        Command::BucketList => buckets(result),
        Command::BucketForget { .. } => {
            done(&format!("Forgot bucket {}. Its disks stay in the bucket", text(&result["forgotten"])));
        }
        Command::Restore { disk, snapshot, target } => {
            done(&format!("Made disk {target} from snapshot {snapshot} of disk {disk}"));
            next("Attach it", &format!("sudo mica disk attach {target}"));
        }
    }
}

fn buckets(result: &Value) {
    let buckets = result["buckets"].as_array().cloned().unwrap_or_default();
    if buckets.is_empty() {
        println!("This machine knows no bucket yet.");
        next("Add one", "sudo mica bucket add --store s3.toml");
        return;
    }
    let rows = buckets
        .iter()
        .map(|bucket| {
            let key = text(&bucket["access_key_id"]);
            let key = format!("{}…", key.chars().take(8).collect::<String>());
            let prefix = bucket["prefix"].as_str().filter(|prefix| !prefix.is_empty()).unwrap_or("-");
            vec![style(text(&bucket["name"])).bold().to_string(), prefix.to_string(), text(&bucket["endpoint"]), key, bucket["attached_here"].to_string()]
        })
        .collect();
    table(&["BUCKET", "PREFIX", "ENDPOINT", "KEY", "ATTACHED HERE"], rows);
}

fn status(result: &Value) {
    let disks = result["disks"].as_array().cloned().unwrap_or_default();
    let count = match disks.len() {
        0 => "No disks attached".to_string(),
        1 => "1 disk attached".to_string(),
        n => format!("{n} disks attached"),
    };
    println!("{} {} on {}. {count}.", style("mica").bold(), text(&result["version"]), text(&result["node"]));
    if result["version"].as_str() != Some(env!("CARGO_PKG_VERSION")) {
        eprintln!(
            "{} the daemon runs another version of mica than this command: restart it with `sudo systemctl restart mica`",
            style("warning:").yellow().bold()
        );
    }
    if disks.is_empty() {
        next("See the disks in your buckets", "sudo mica disk ls");
        return;
    }
    println!();
    let rows = disks
        .iter()
        .map(|disk| {
            let (status, rates) = (&disk["status"], &disk["rates"]);
            vec![
                style(text(&disk["disk"])).bold().to_string(),
                text(&disk["device"]),
                rate(&rates["read"]),
                rate(&rates["written"]),
                rate(&rates["uploaded"]),
                rate(&rates["downloaded"]),
                used(&status["unsaved_bytes"], &status["limits"]["unsaved_bytes"]),
                used(&status["cache_bytes"], &status["limits"]["cache_bytes"]),
                ago(&status["saved_time"]),
                state(disk),
            ]
        })
        .collect();
    table(&["DISK", "DEVICE", "READ", "WRITE", "UPLOAD", "DOWNLOAD", "UNSAVED", "CACHE", "LAST SAVE", "STATE"], rows);
}

fn list(result: &Value) {
    let disks = result["disks"].as_array().cloned().unwrap_or_default();
    let buckets: Vec<&str> = result["buckets"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    for (number, bucket) in buckets.iter().enumerate() {
        if number > 0 {
            println!();
        }
        println!("{} {}", style("Bucket").bold(), style(bucket).dim());
        let rows: Vec<Vec<String>> = disks
            .iter()
            .filter(|disk| disk["bucket"].as_str() == Some(bucket))
            .map(|disk| {
                let local = &disk["local"]["status"];
                let snapshots = disk["snapshots"].as_array().map_or(0, Vec::len);
                vec![
                    style(text(&disk["disk"])).bold().to_string(),
                    size(&disk["size"]),
                    place(disk),
                    local.get("unsaved_bytes").map_or("-".into(), size),
                    size(&disk["cache_bytes"]),
                    ago(&disk["time"]),
                    snapshots.to_string(),
                ]
            })
            .collect();
        match rows.is_empty() {
            true => println!("  No disks yet."),
            false => table(&["DISK", "SIZE", "WHERE", "UNSAVED", "CACHE", "LAST SAVE", "SNAPSHOTS"], rows),
        }
    }
    if disks.is_empty() {
        next("Make one", "sudo mica disk create <disk> --size 50G");
    }
}

fn info(result: &Value) {
    let name = text(&result["disk"]);
    let limits = &result["limits"];
    println!("{} {}", style("Disk").bold(), style(&name).bold());
    let mut fields = vec![
        ("Where", place(result)),
        ("Size", format!("{} in chunks of {}", size(&result["size"]), size(&result["chunk_size"]))),
        ("Last save", ago(&result["time"])),
    ];
    if let Some(local) = result["local"].get("status") {
        fields.push(("Unsaved", used(&local["unsaved_bytes"], &limits["unsaved_bytes"])));
        fields.push(("Cache", used(&local["cache_bytes"], &limits["cache_bytes"])));
    } else {
        fields.push(("Unsaved", format!("up to {}", size(&limits["unsaved_bytes"]))));
        fields.push(("Cache", size(&limits["cache_bytes"])));
    }
    fields.push(("Saves", format!("every {}", interval(&limits["interval_seconds"]))));
    fields.push(("Bucket", text(&result["bucket"])));
    for (label, value) in fields {
        println!("  {}  {value}", style(format!("{label:<9}")).dim());
    }
    if result["deleted"].as_bool() == Some(true) {
        eprintln!("{} the delete of this disk did not finish: run `sudo mica disk delete {name}`", style("warning:").yellow().bold());
    }
    println!();
    let snapshots = result["snapshots"].as_array().cloned().unwrap_or_default();
    if snapshots.is_empty() {
        println!("No snapshots.");
        next("Take one", &format!("sudo mica snapshot create {name} <snapshot>"));
        return;
    }
    let rows = snapshots
        .iter()
        .map(|snapshot| {
            let deleting = snapshot["deleting"].as_bool() == Some(true);
            vec![style(text(&snapshot["name"])).bold().to_string(), ago(&snapshot["time"]), if deleting { "deleting".into() } else { String::new() }]
        })
        .collect();
    table(&["SNAPSHOT", "TAKEN", ""], rows);
}

fn place(disk: &Value) -> String {
    if let Some(device) = disk["local"]["device"].as_str() {
        return format!("{} here: {device}", style("●").green());
    }
    match disk["owner"].as_str() {
        Some(machine) => format!("{} on {machine}", style("●").yellow()),
        None => format!("{} detached", style("○").dim()),
    }
}

fn state(disk: &Value) -> String {
    let status = &disk["status"];
    if let Some(reason) = status["stopped"].as_str() {
        return style(format!("stopped: {reason}")).red().bold().to_string();
    }
    if disk["detaching"].as_bool() == Some(true) {
        return style("detaching").cyan().to_string();
    }
    if status["behind"].as_bool() == Some(true) {
        return style("upload behind").yellow().to_string();
    }
    style("ok").green().to_string()
}

fn warn(warning: &Value) {
    if let Some(warning) = warning.as_str() {
        eprintln!("{} {warning}", style("warning:").yellow().bold());
    }
}

fn done(message: &str) {
    println!("{} {message}", style("✔").green().bold());
}

fn next(what: &str, command: &str) {
    println!("  {what}: {}", style(command).cyan());
}

fn table(header: &[&str], rows: Vec<Vec<String>>) {
    let mut widths: Vec<usize> = header.iter().map(|title| title.len()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(measure_text_width(cell));
        }
    }
    let line = |cells: Vec<String>| {
        let padded: Vec<String> = cells.iter().zip(&widths).map(|(cell, &width)| pad_str(cell, width, Alignment::Left, None).into_owned()).collect();
        println!("{}", padded.join("  ").trim_end());
    };
    line(header.iter().map(|title| style(title).dim().to_string()).collect());
    rows.into_iter().for_each(line);
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or("-").to_string()
}

fn size(value: &Value) -> String {
    format_size(value.as_u64().unwrap_or(0))
}

fn used(value: &Value, limit: &Value) -> String {
    format!("{} / {}", size(value), size(limit))
}

fn rate(value: &Value) -> String {
    match value.as_u64() {
        Some(0) | None => style("-").dim().to_string(),
        Some(bytes) => format!("{}/s", format_size(bytes)),
    }
}

fn interval(seconds: &Value) -> String {
    seconds.as_u64().map(format_duration).unwrap_or_else(|| "-".into())
}

fn ago(time: &Value) -> String {
    let Some(time) = time.as_u64().filter(|&time| time > 0) else { return "never".into() };
    let seconds = unix_now().saturating_sub(time);
    match seconds {
        0..60 => format!("{seconds} s ago"),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86400 => format!("{} h ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86400),
    }
}
