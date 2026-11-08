//! Model tests of the disk engine against a store in memory.
//!
//! After each step, two facts must hold. Each object that the current
//! version or a snapshot needs exists. And no other object exists.

use super::*;
use crate::local::NodeCache;
use crate::owner::{Claim, NodeId};
use crate::store::{Bucket, DiskStore, Head, Limits, Owner};
use std::collections::BTreeSet;

const CHUNK: u64 = 64 << 10;
const SIZE: u64 = 4096 * CHUNK;

struct Rig {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    bucket: Arc<Bucket>,
    node: NodeId,
    cache: Arc<NodeCache>,
}

impl Rig {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let cache = NodeCache::load(root.join("cache")).unwrap();
        let node = NodeId { hostname: "test".into(), id: "node-a".into() };
        Self { _dir: dir, root, bucket: Arc::new(Bucket::in_memory()), node, cache }
    }

    fn store(&self, name: &str) -> DiskStore {
        DiskStore::new(self.bucket.clone(), name)
    }

    async fn create(&self, name: &str) {
        let limits = Limits { cache_bytes: 4 * CHUNK, unsaved_bytes: 1024 * CHUNK, interval_seconds: 1 };
        let mut claim = Claim::create(self.store(name), &self.node, Head::new(SIZE, CHUNK, limits, None)).await.unwrap();
        claim.release().await.unwrap();
    }

    async fn attach(&self, name: &str) -> Arc<Disk> {
        let claim = Claim::take(self.store(name), &self.node).await.unwrap();
        let folder = DiskFolder::new(&self.root.join("data"), &self.root.join("run"), name);
        folder.create(&claim.head().id).unwrap();
        let cache = self.cache.for_disk(name, 64 * CHUNK);
        Disk::open(claim, &folder, cache, Transfers::new()).await.unwrap()
    }

    async fn detach(&self, disk: Arc<Disk>) {
        disk.transaction(TransactionKind::All, None).await.unwrap();
        disk.close();
        disk.claim().await.release().await.unwrap();
        DiskFolder::new(&self.root.join("data"), &self.root.join("run"), &disk.name).remove().unwrap();
    }

    /// Checks that the objects in the store are exactly the needed ones.
    async fn check(&self, name: &str) {
        let store = self.store(name);
        let head = store.require_head().await.unwrap().head;
        let mut needed = BTreeSet::from([format!("d/{name}/head")]);
        let versions = head.snapshots.values().map(|snapshot| snapshot.transaction).chain([head.transaction]);
        for version in versions.filter(|&version| version != 0) {
            let root = store.root(&head, version).await.unwrap();
            needed.insert(store.root_key(version));
            for (index, entry) in root.pages.iter().enumerate().filter(|(_, entry)| !entry.is_zero()) {
                needed.insert(store.page_key(entry.birth, index));
                let page = store.page(entry, index).await.unwrap();
                for (offset, chunk) in page.0.iter().enumerate().filter(|(_, chunk)| !chunk.is_zero()) {
                    needed.insert(store.chunk_key(chunk.birth, index * 1024 + offset));
                }
            }
        }
        let found: BTreeSet<String> = self.bucket.list(&store.prefix()).await.unwrap().into_iter().collect();
        assert_eq!(found, needed, "objects in the store differ from the needed ones");
    }
}

fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| seed.wrapping_add((i % 251) as u8)).collect()
}

#[tokio::test]
async fn writes_survive_a_detach_and_garbage_goes_at_once() {
    let rig = Rig::new();
    rig.create("web").await;
    let disk = rig.attach("web").await;
    assert!(disk.is_blank());
    disk.write(0, pattern(1, 4096)).await.unwrap();
    disk.write(CHUNK * 1500 + 100, pattern(2, CHUNK as usize)).await.unwrap();
    disk.flush().await.unwrap();
    assert_eq!(disk.read(0, 4096).await.unwrap(), pattern(1, 4096));
    disk.transaction(TransactionKind::All, None).await.unwrap();
    rig.check("web").await;
    // The second write replaces chunk 0: the old object goes at once.
    disk.write(0, pattern(3, 4096)).await.unwrap();
    disk.transaction(TransactionKind::All, None).await.unwrap();
    rig.check("web").await;
    rig.detach(disk).await;

    let disk = rig.attach("web").await;
    assert_eq!(disk.read(0, 4096).await.unwrap(), pattern(3, 4096));
    assert_eq!(disk.read(CHUNK * 1500 + 100, CHUNK as usize).await.unwrap(), pattern(2, CHUNK as usize));
    assert_eq!(disk.read(CHUNK * 7, 512).await.unwrap(), vec![0; 512]);
    rig.detach(disk).await;
    rig.check("web").await;
}

#[tokio::test]
async fn the_status_of_a_disk_with_unsaved_data_returns() {
    let rig = Rig::new();
    rig.create("web").await;
    let disk = rig.attach("web").await;
    disk.write(0, pattern(1, 4096)).await.unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let copy = disk.clone();
    std::thread::spawn(move || sender.send(copy.status().unsaved_bytes));
    assert_eq!(receiver.recv_timeout(std::time::Duration::from_secs(5)).expect("status deadlocks"), CHUNK);
    rig.detach(disk).await;
}

#[tokio::test]
async fn snapshots_keep_their_objects_until_the_merge() {
    let rig = Rig::new();
    rig.create("db").await;
    let disk = rig.attach("db").await;
    disk.write(0, pattern(1, CHUNK as usize * 3)).await.unwrap();
    disk.transaction(TransactionKind::All, Some("one")).await.unwrap();
    disk.write(CHUNK, pattern(2, CHUNK as usize)).await.unwrap();
    disk.transaction(TransactionKind::All, Some("two")).await.unwrap();
    disk.write(0, pattern(3, CHUNK as usize * 2)).await.unwrap();
    disk.transaction(TransactionKind::All, None).await.unwrap();
    disk.discard(0, CHUNK * 3).await.unwrap();
    disk.transaction(TransactionKind::All, None).await.unwrap();
    rig.check("db").await;
    assert_eq!(disk.read(0, 16).await.unwrap(), vec![0; 16]);

    disk.claim().await.delete_snapshot("one").await.unwrap();
    rig.check("db").await;
    disk.claim().await.delete_snapshot("two").await.unwrap();
    rig.check("db").await;
    let head = rig.store("db").require_head().await.unwrap().head;
    assert!(head.snapshots.is_empty());
    rig.detach(disk).await;
}

#[tokio::test]
async fn a_snapshot_of_an_unchanged_disk_names_the_current_version() {
    let rig = Rig::new();
    rig.create("idle").await;
    let disk = rig.attach("idle").await;
    disk.write(0, pattern(5, 4096)).await.unwrap();
    let version = disk.transaction(TransactionKind::All, None).await.unwrap();
    assert_eq!(disk.transaction(TransactionKind::All, Some("same")).await.unwrap(), version);
    let head = disk.claim().await.head().clone();
    assert_eq!(head.snapshots["same"].transaction, version);
    disk.write(0, pattern(6, 4096)).await.unwrap();
    disk.transaction(TransactionKind::All, None).await.unwrap();
    rig.check("idle").await;
    rig.detach(disk).await;
}

#[tokio::test]
async fn a_changed_head_stops_the_owner() {
    let rig = Rig::new();
    rig.create("moved").await;
    let disk = rig.attach("moved").await;
    disk.write(0, pattern(1, 4096)).await.unwrap();
    let other = NodeId { hostname: "other".into(), id: "node-b".into() };
    assert!(Claim::take(rig.store("moved"), &other).await.is_err());
    // Another writer of the head, such as an orchestrator, gives the disk to another node.
    let store = rig.store("moved");
    let current = store.require_head().await.unwrap();
    let mut moved = current.head.clone();
    moved.owner = Some(Owner { node: other.hostname.clone(), id: other.id.clone(), since: 0 });
    store.replace_head(&current, moved).await.unwrap();
    assert!(disk.transaction(TransactionKind::All, None).await.is_err());
    assert!(disk.is_stopped());
    assert!(disk.read(0, 4096).await.is_err());
}

#[tokio::test]
async fn a_release_gives_the_disk_of_a_dead_node_free() {
    let rig = Rig::new();
    rig.create("stuck").await;
    let disk = rig.attach("stuck").await;
    disk.write(0, pattern(2, 4096)).await.unwrap();
    disk.transaction(TransactionKind::All, None).await.unwrap();
    let other = NodeId { hostname: "other".into(), id: "node-b".into() };
    assert!(Claim::release_from(&rig.store("stuck"), &rig.node).await.is_err(), "the owner detaches instead");
    assert_eq!(Claim::release_from(&rig.store("stuck"), &other).await.unwrap(), "test");
    // The old node still runs: it stops at its next head write.
    disk.write(0, pattern(3, 4096)).await.unwrap();
    assert!(disk.transaction(TransactionKind::All, None).await.is_err());
    let mut claim = Claim::take(rig.store("stuck"), &other).await.unwrap();
    claim.release().await.unwrap();
    rig.check("stuck").await;
}

#[tokio::test]
async fn local_chunks_come_back_after_a_crash() {
    let rig = Rig::new();
    rig.create("crash").await;
    let disk = rig.attach("crash").await;
    disk.write(CHUNK * 9, pattern(4, 8192)).await.unwrap();
    disk.flush().await.unwrap();
    // A crash: the disk goes away without a transaction or a release.
    disk.close();
    drop(disk);
    let disk = rig.attach("crash").await;
    assert_eq!(disk.read(CHUNK * 9, 8192).await.unwrap(), pattern(4, 8192));
    assert!(disk.has_unsaved());
    rig.detach(disk).await;
    rig.check("crash").await;
}

/// A small random number generator, so that a failure repeats with its seed.
struct Random(u64);

impl Random {
    fn below(&mut self, limit: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % limit
    }
}

/// Random partial writes, flushes, transactions and restarts of the daemon.
/// After each step, the disk reads the same bytes as a plain array.
#[tokio::test]
async fn random_steps_never_change_what_the_disk_reads() {
    const CHUNKS: u64 = 48;
    for seed in 1..=6u64 {
        let rig = Rig::new();
        rig.create("model").await;
        let mut disk = rig.attach("model").await;
        let mut model = vec![0u8; (CHUNKS * CHUNK) as usize];
        let mut random = Random(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        for step in 0..400 {
            match random.below(10) {
                0..=5 => {
                    let offset = random.below(CHUNKS * CHUNK / 512) * 512;
                    let len = ((random.below(3 * CHUNK / 512) + 1) * 512).min(CHUNKS * CHUNK - offset);
                    let data = pattern((step % 251) as u8, len as usize);
                    model[offset as usize..(offset + len) as usize].copy_from_slice(&data);
                    disk.write(offset, data).await.unwrap();
                }
                6 => disk.flush().await.unwrap(),
                7 | 8 => drop(disk.transaction(TransactionKind::All, None).await.unwrap()),
                _ => {
                    // A restart: the daemon exits after a flush, and the next one opens the folder.
                    disk.flush().await.unwrap();
                    drop(disk);
                    disk = rig.attach("model").await;
                    assert!(disk.read(0, model.len()).await.unwrap() == model, "seed {seed}, step {step}: lost at a restart");
                }
            }
            if step % 25 == 0 {
                let read = disk.read(0, model.len()).await.unwrap();
                if let Some(at) = read.iter().zip(&model).position(|(a, b)| a != b) {
                    panic!("seed {seed}, step {step}: byte {at} (chunk {}) differs", at as u64 / CHUNK);
                }
            }
        }
        rig.detach(disk).await;
        let disk = rig.attach("model").await;
        assert!(disk.read(0, model.len()).await.unwrap() == model, "seed {seed}: the store lost data");
    }
}

/// Writes one block, on the fast path of the queue threads when it can.
async fn write_block(disk: &Disk, offset: u64, data: Vec<u8>) {
    if let Some(plan) = disk.plan_local_write(offset, data.len()) {
        for (part, file, at) in &plan.parts {
            std::os::unix::fs::FileExt::write_all_at(&**file, &data[part.buffer.clone()], *at).unwrap();
        }
        return plan.finish();
    }
    disk.write(offset, data).await.unwrap();
}

async fn read_block(disk: &Disk, offset: u64, len: usize) -> Vec<u8> {
    if let Some(plan) = disk.plan_local_read(offset, len) {
        let mut buffer = vec![0u8; len];
        for (part, source) in &plan.parts {
            if let fast::ReadSource::File(file, at) = source {
                std::os::unix::fs::FileExt::read_exact_at(&**file, &mut buffer[part.buffer.clone()], *at).unwrap();
            }
        }
        return buffer;
    }
    disk.read(offset, len).await.unwrap()
}

/// Four writers share chunks, but each owns its own blocks. Transactions,
/// write-back and the eviction of saved chunks run at the same time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_and_transactions_keep_every_block() {
    const BLOCK: u64 = 4096;
    const BLOCKS: u64 = 32 * CHUNK / BLOCK;
    let rig = Rig::new();
    rig.create("busy").await;
    let disk = rig.attach("busy").await;
    let transactions = tokio::spawn(disk.clone().run_transactions());
    let writeback = tokio::spawn(disk.clone().run_writeback());
    let mut writers = Vec::new();
    for writer in 0..4u64 {
        let disk = disk.clone();
        writers.push(tokio::spawn(async move {
            let mut model = std::collections::HashMap::new();
            let mut random = Random((writer + 1).wrapping_mul(0x2545_F491_4F6C_DD1D));
            for step in 0..1500u64 {
                let block = random.below(BLOCKS / 4) * 4 + writer;
                let data = pattern((step * 4 + writer) as u8, BLOCK as usize);
                write_block(&disk, block * BLOCK, data.clone()).await;
                assert!(read_block(&disk, block * BLOCK, BLOCK as usize).await == data, "writer {writer}, step {step}: block {block}");
                model.insert(block, data);
                if random.below(20) == 0 {
                    disk.flush().await.unwrap();
                }
                if random.below(50) == 0 {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
            }
            model
        }));
    }
    let mut model = std::collections::HashMap::new();
    for writer in writers {
        model.extend(writer.await.unwrap());
    }
    let check = async |disk: &Disk, when: &str| {
        for (block, data) in &model {
            assert!(read_block(disk, block * BLOCK, BLOCK as usize).await == *data, "{when}: block {block} (chunk {})", block * BLOCK / CHUNK);
        }
    };
    check(&disk, "after the writes").await;
    disk.flush().await.unwrap();
    disk.close();
    transactions.abort();
    writeback.abort();
    drop(disk);
    let disk = rig.attach("busy").await;
    check(&disk, "after a restart").await;
    rig.detach(disk).await;
    let disk = rig.attach("busy").await;
    check(&disk, "after a detach").await;
    rig.detach(disk).await;
    rig.check("busy").await;
}

#[tokio::test]
async fn the_probe_finds_the_conditional_writes_of_a_store() {
    let bucket = Bucket::in_memory();
    assert!(bucket.check_conditional_writes().await.unwrap());
    assert!(bucket.list("").await.unwrap().is_empty(), "the probe stays in the store");
    assert!(!Bucket::in_memory_without_conditions().check_conditional_writes().await.unwrap());
}

#[tokio::test]
async fn a_store_without_conditions_keeps_an_existing_disk() {
    let store = DiskStore::new(Arc::new(Bucket::in_memory_without_conditions()), "a");
    let limits = Limits { cache_bytes: 4 * CHUNK, unsaved_bytes: 1024 * CHUNK, interval_seconds: 1 };
    store.create_head(&Head::new(SIZE, CHUNK, limits, None)).await.unwrap();
    assert!(store.create_head(&Head::new(SIZE, CHUNK, limits, None)).await.is_err());
}

#[tokio::test]
async fn a_store_without_conditions_refuses_a_head_write_after_a_change() {
    let store = DiskStore::new(Arc::new(Bucket::in_memory_without_conditions()), "a");
    let limits = Limits { cache_bytes: 4 * CHUNK, unsaved_bytes: 1024 * CHUNK, interval_seconds: 1 };
    let first = store.create_head(&Head::new(SIZE, CHUNK, limits, None)).await.unwrap();
    let mut changed = first.head.clone();
    changed.next += 1;
    store.replace_head(&first, changed.clone()).await.unwrap();
    assert!(store.replace_head(&first, changed).await.is_err(), "a stale head must not win");
}
