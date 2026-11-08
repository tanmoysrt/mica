//! The throughput of each attached disk, for `mica status`. The daemon
//! samples the counters once a second, and a rate is the change over the
//! last few seconds.

use crate::store::Bucket;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::Instant;

const WINDOW: usize = 5;

#[derive(Default)]
pub struct Meter {
    samples: VecDeque<(Instant, Counters)>,
}

/// Bytes since the attach. Rates in bytes each second have the same fields.
#[derive(Clone, Copy, Default, Serialize)]
pub struct Counters {
    pub read: u64,
    pub written: u64,
    pub uploaded: u64,
    pub downloaded: u64,
}

impl Meter {
    /// `device` is the kernel name of the block device, such as `ublkb0`.
    pub fn sample(&mut self, device: Option<&str>, bucket: &Bucket) {
        let (read, written) = device.and_then(device_bytes).unwrap_or_default();
        let counts = &bucket.counts;
        let (uploaded, downloaded) = (counts.bytes_out.load(Ordering::Relaxed), counts.bytes_in.load(Ordering::Relaxed));
        if self.samples.len() == WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back((Instant::now(), Counters { read, written, uploaded, downloaded }));
    }

    pub fn rates(&self) -> Counters {
        let (Some((start, old)), Some((end, new))) = (self.samples.front(), self.samples.back()) else { return Counters::default() };
        let seconds = end.duration_since(*start).as_secs_f64();
        if seconds == 0.0 {
            return Counters::default();
        }
        let rate = |old: u64, new: u64| (new.saturating_sub(old) as f64 / seconds) as u64;
        Counters {
            read: rate(old.read, new.read),
            written: rate(old.written, new.written),
            uploaded: rate(old.uploaded, new.uploaded),
            downloaded: rate(old.downloaded, new.downloaded),
        }
    }
}

/// The bytes read and written, from the sector counts that the kernel keeps.
fn device_bytes(name: &str) -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string(format!("/sys/block/{name}/stat")).ok()?;
    let fields: Vec<u64> = stat.split_whitespace().filter_map(|field| field.parse().ok()).collect();
    Some((fields.get(2)? << 9, fields.get(6)? << 9))
}
