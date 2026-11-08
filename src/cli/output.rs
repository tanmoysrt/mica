//! Human output of the CLI. `--json` prints the answer of the daemon instead.

use crate::config::format_size;
use crate::daemon::api::Request;
use serde_json::Value;

pub fn print(request: &Request, result: &Value) {
    let size = |value: &Value| format_size(value.as_u64().unwrap_or(0));
    match request {
        Request::Status => {
            println!("Node {}", text(&result["node"]));
            let rows = rows(&result["disks"], |disk| {
                let status = &disk["status"];
                let note = match (status["stopped"].as_str(), status["behind"].as_bool(), disk["detaching"].as_bool()) {
                    (Some(reason), _, _) => format!("stopped: {reason}"),
                    (_, _, Some(true)) => "detaching".into(),
                    (_, Some(true), _) => "upload is behind".into(),
                    _ => String::new(),
                };
                vec![text(&disk["disk"]), text(&disk["device"]), size(&status["size"]), size(&status["unsaved_bytes"]), size(&status["cache_bytes"]), note]
            });
            table(&["DISK", "DEVICE", "SIZE", "UNSAVED", "CACHE", "NOTE"], rows);
        }
        Request::List { .. } => {
            let rows = rows(&result["disks"], |disk| {
                let owner = disk["owner"].as_str().unwrap_or("-").to_string();
                let snapshots = disk["snapshots"].as_array().map_or(0, Vec::len).to_string();
                vec![text(&disk["disk"]), size(&disk["size"]), owner, snapshots, ago(&disk["time"])]
            });
            table(&["DISK", "SIZE", "ATTACHED ON", "SNAPSHOTS", "LAST TRANSACTION"], rows);
        }
        Request::Info { .. } => {
            println!("Disk:          {}", text(&result["disk"]));
            println!("Size:          {} in chunks of {}", size(&result["size"]), size(&result["chunk_size"]));
            println!("Attached on:   {}", result["owner"].as_str().unwrap_or("-"));
            println!("Transaction:   {} ({})", result["transaction"], ago(&result["time"]));
            println!("Cache budget:  {}", size(&result["limits"]["cache_bytes"]));
            println!("Unsaved limit: {}", size(&result["limits"]["unsaved_bytes"]));
            println!("Interval:      {}", interval(&result["limits"]["interval_seconds"]));
            println!();
            let rows = rows(&result["snapshots"], |snapshot| {
                let name = text(&snapshot["name"]) + if snapshot["deleting"].as_bool() == Some(true) { " (deleting)" } else { "" };
                vec![name, snapshot["transaction"].to_string(), ago(&snapshot["time"])]
            });
            table(&["SNAPSHOT", "TRANSACTION", "TAKEN"], rows);
        }
        Request::Attach { disk, .. } => {
            println!("Attached disk {disk} as {}", text(&result["device"]));
            if result["blank"].as_bool() == Some(true) {
                println!("The disk is new: make a filesystem on it, such as `mkfs.ext4 {}`", text(&result["device"]));
            }
        }
        Request::Detach { disk } => println!("Detached disk {disk}. The bucket has all its data"),
        Request::Create { disk, .. } => {
            println!("Made disk {disk}: {}", size(&result["size"]));
            if let Some(warning) = result["warning"].as_str() {
                println!("Warning: {warning}");
            }
        }
        Request::Restore { target, .. } => println!("Made disk {target} from the snapshot: {} objects copied", result["copied_objects"]),
        Request::SnapshotCreate { disk, snapshot, .. } => println!("Took snapshot {snapshot} of disk {disk} at transaction {}", result["transaction"]),
        Request::SnapshotDelete { snapshot, .. } => println!("Deleted snapshot {snapshot}"),
        Request::Release { disk, .. } => println!("Released disk {disk} from {}. Its unsaved data there is lost", text(&result["released_from"])),
        Request::Delete { disk, .. } => println!("Deleted disk {disk}"),
        Request::Set { disk, .. } => println!(
            "Disk {disk}: size {}, cache budget {}, unsaved limit {}, interval {}",
            size(&result["size"]),
            size(&result["limits"]["cache_bytes"]),
            size(&result["limits"]["unsaved_bytes"]),
            interval(&result["limits"]["interval_seconds"])
        ),
    }
}

fn rows(list: &Value, row: impl Fn(&Value) -> Vec<String>) -> Vec<Vec<String>> {
    list.as_array().map(|items| items.iter().map(row).collect()).unwrap_or_default()
}

fn table(header: &[&str], rows: Vec<Vec<String>>) {
    let mut widths: Vec<usize> = header.iter().map(|title| title.len()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    let line = |cells: Vec<String>| {
        let padded: Vec<String> = cells.iter().zip(&widths).map(|(cell, width)| format!("{cell:width$}")).collect();
        println!("{}", padded.join("   ").trim_end());
    };
    line(header.iter().map(|title| title.to_string()).collect());
    rows.into_iter().for_each(line);
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or("-").to_string()
}

fn interval(seconds: &Value) -> String {
    seconds.as_u64().map(crate::config::format_duration).unwrap_or_else(|| "-".into())
}

fn ago(time: &Value) -> String {
    let Some(time) = time.as_u64().filter(|&time| time > 0) else { return "never".into() };
    let seconds = crate::owner::unix_now().saturating_sub(time);
    match seconds {
        0..60 => format!("{seconds} s ago"),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86400 => format!("{} h ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86400),
    }
}
