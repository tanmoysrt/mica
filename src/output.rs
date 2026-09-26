use serde_json::Value;

/// Prints rows as aligned columns under a header, spaced like `docker ps`.
pub fn print_table(header: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = header.iter().map(|title| title.len()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    let line = |cells: Vec<&str>| {
        let padded: Vec<String> = cells.iter().zip(&widths).map(|(cell, width)| format!("{cell:<width$}")).collect();
        println!("{}", padded.join("   ").trim_end());
    };
    line(header.to_vec());
    for row in rows {
        line(row.iter().map(String::as_str).collect());
    }
}

/// `mica status`: the disks attached on this node.
pub fn print_status(status: &Value) {
    let disks = status["disks"].as_array().cloned().unwrap_or_default();
    let rows: Vec<Vec<String>> = disks
        .iter()
        .map(|disk| {
            let details = &disk["status"];
            vec![
                text(&disk["disk"]),
                text(&disk["state"]),
                disk["device"].as_str().unwrap_or("-").to_string(),
                bytes(details["size"].as_u64().unwrap_or(0)),
                bytes(details["unsaved_bytes"].as_u64().unwrap_or(0)),
                note(details),
            ]
        })
        .collect();
    print_table(&["DISK", "STATE", "DEVICE", "SIZE", "NOT IN S3", "NOTE"], &rows);
    print_request_counts(&status["s3"]);
}

/// Class A requests (PUT, LIST) cost more than class B (GET, HEAD).
fn print_request_counts(counts: &Value) {
    let number = |key: &str| counts[key].as_u64().unwrap_or(0);
    println!();
    println!("S3 requests since mica started:");
    print_fields(&[
        ("  Class A", format!("{} PUT, {} LIST", number("put"), number("list"))),
        ("  Class B", format!("{} GET, {} HEAD", number("get"), number("head"))),
        ("  Free", format!("{} DELETE", number("delete"))),
        ("  Data", format!("{} downloaded, {} uploaded", bytes(number("bytes_in")), bytes(number("bytes_out")))),
    ]);
}

/// `mica disk ls`: every disk in the bucket.
pub fn print_disks(disks: &[Value]) {
    let rows: Vec<Vec<String>> = disks
        .iter()
        .map(|disk| {
            vec![
                text(&disk["disk"]),
                bytes(disk["size"].as_u64().unwrap_or(0)),
                disk["attached_on"].as_str().unwrap_or("-").to_string(),
                age(disk["time"].as_str().unwrap_or("")),
            ]
        })
        .collect();
    print_table(&["DISK", "SIZE", "ATTACHED ON", "LAST SAVED"], &rows);
}

/// `mica snapshot ls`: every snapshot in the bucket.
pub fn print_snapshots(snapshots: &[Value]) {
    let rows: Vec<Vec<String>> = snapshots
        .iter()
        .map(|snapshot| {
            vec![
                text(&snapshot["name"]),
                bytes(snapshot["size"].as_u64().unwrap_or(0)),
                text(&snapshot["disk"]),
                snapshot["time"].as_str().map(age).unwrap_or_else(|| "-".to_string()),
            ]
        })
        .collect();
    print_table(&["SNAPSHOT", "SIZE", "FROM DISK", "CREATED"], &rows);
}

/// `mica cache status`: one item, so labels and values instead of a table.
pub fn print_cache_usage(usage: &Value) {
    let number = |key: &str| usage[key].as_u64().unwrap_or(0);
    print_fields(&[
        ("Used", format!("{} of {}", bytes(number("bytes")), bytes(number("limit_bytes")))),
        ("Chunks", number("chunks").to_string()),
        ("Used by attached disks", bytes(number("in_use_bytes"))),
        ("Unused", bytes(number("bytes") - number("in_use_bytes"))),
    ]);
}

/// Prints `label:   value` lines with the values aligned.
pub fn print_fields(fields: &[(&str, String)]) {
    let width = fields.iter().map(|(label, _)| label.len() + 1).max().unwrap_or(0);
    for (label, value) in fields {
        println!("{:<width$}   {value}", format!("{label}:"));
    }
}

/// `mica gc` before it deletes: what stays, and what would go.
pub fn print_gc_preview(preview: &Value) {
    let report = &preview["report"];
    let number = |key: &str| report[key].as_u64().unwrap_or(0);
    println!(
        "Kept: {} disks, {} snapshots, {} manifests, {} chunks",
        number("disks"),
        number("snapshots"),
        number("kept_manifests"),
        number("kept_chunks")
    );
    if number("garbage_chunks") == 0 && number("garbage_manifests") == 0 {
        println!("Nothing to delete. Objects written in the last {} are kept.", preview["grace"].as_str().unwrap_or("?"));
        return;
    }
    println!(
        "Garbage: {} chunks ({}) and {} manifests ({})",
        number("garbage_chunks"),
        bytes(number("garbage_chunk_bytes")),
        number("garbage_manifests"),
        bytes(number("garbage_manifest_bytes"))
    );
}

pub fn print_gc_result(report: &Value) {
    let number = |key: &str| report[key].as_u64().unwrap_or(0);
    println!(
        "Deleted {} chunks and {} manifests, and freed {}",
        number("garbage_chunks"),
        number("garbage_manifests"),
        bytes(number("garbage_chunk_bytes") + number("garbage_manifest_bytes"))
    );
}

/// Sizes in binary units, as `df -h` shows them.
pub fn bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 || size.fract() == 0.0 {
        format!("{size:.0}{}", UNITS[unit])
    } else {
        format!("{size:.1}{}", UNITS[unit])
    }
}

/// An age in the words `docker ps` uses. It reads better than a UTC
/// timestamp in another time zone.
fn age(rfc3339: &str) -> String {
    let Ok(time) = humantime::parse_rfc3339(rfc3339) else { return "-".to_string() };
    let seconds = std::time::SystemTime::now().duration_since(time).map(|age| age.as_secs()).unwrap_or(0);
    let (minutes, hours, days) = (seconds / 60, seconds / 3600, seconds / 86400);
    match seconds {
        0..60 => "Less than a minute ago".to_string(),
        60..120 => "About a minute ago".to_string(),
        120..3600 => format!("{minutes} minutes ago"),
        3600..7200 => "About an hour ago".to_string(),
        7200..172800 => format!("{hours} hours ago"),
        172800..1209600 => format!("{days} days ago"),
        1209600..5184000 => format!("{} weeks ago", days / 7),
        _ => format!("{} months ago", days / 30),
    }
}

fn note(details: &Value) -> String {
    if details["ownership_lost"] == true {
        "taken by another node".to_string()
    } else if details["sync_failed"] == true {
        "stopped: local sync failed".to_string()
    } else if details["behind"] == true {
        "upload is behind".to_string()
    } else {
        String::new()
    }
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or("-").to_string()
}
