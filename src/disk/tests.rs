//! Model tests of the disk engine against a store in memory.
//!
//! After each step, two facts must hold. Each object that the current
//! version or a snapshot needs exists. And no other object exists.

use super::*;
use crate::local::NodeCache;
use crate::owner::{Claim, NodeId};
use crate::store::{Bucket, DiskStore, Head, Limits};
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
        let limits = Limits { cache_bytes: 64 * CHUNK, unsaved_bytes: 1024 * CHUNK };
        let mut claim = Claim::create(self.store(name), &self.node, Head::new(SIZE, CHUNK, limits, None)).await.unwrap();
        claim.release().await.unwrap();
    }

    async fn attach(&self, name: &str) -> Arc<Disk> {
        let claim = Claim::take(self.store(name), &self.node, false).await.unwrap();
        let folder = DiskFolder::new(&self.root.join("data"), &self.root.join("run"), name);
        folder.create(&crate::store::StoreConfig { endpoint: "memory".into(), bucket: "test".into(), ..Default::default() }).unwrap();
        let tuning = Tuning {
            transaction_interval: Duration::from_secs(3600),
            transaction_after: 1 << 30,
            max_unsaved_age: Duration::from_secs(300),
        };
        let cache = self.cache.for_disk(name, 64 * CHUNK);
        Disk::open(claim, &folder, cache, Transfers::new(), tuning).await.unwrap()
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
async fn a_forced_claim_stops_the_old_owner() {
    let rig = Rig::new();
    rig.create("moved").await;
    let disk = rig.attach("moved").await;
    disk.write(0, pattern(1, 4096)).await.unwrap();
    let other = NodeId { hostname: "other".into(), id: "node-b".into() };
    assert!(Claim::take(rig.store("moved"), &other, false).await.is_err());
    let mut taken = Claim::take(rig.store("moved"), &other, true).await.unwrap();
    assert!(disk.transaction(TransactionKind::All, None).await.is_err());
    assert!(disk.is_stopped());
    assert!(disk.read(0, 4096).await.is_err());
    // The new owner deletes the folder of the old owner at its release.
    taken.release().await.unwrap();
    rig.check("moved").await;
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
