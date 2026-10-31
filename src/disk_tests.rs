use super::*;
use crate::bucket::keys;
use crate::manifest::MIN_CHUNK_SIZE;
use crate::records::AttachedMarker;

const CHUNK: u64 = MIN_CHUNK_SIZE;

async fn open_disk(root: &std::path::Path) -> Disk {
    let bucket = Arc::new(Bucket::in_memory());
    let manifest = Manifest::empty("test", 8 * CHUNK, CHUNK).unwrap();
    let manifest_hash = manifest.save(&bucket).await.unwrap();
    let node = Arc::new(NodeIdentity { id: "test".into(), hostname: "test".into() });
    AttachedMarker::for_node(&node).save(&bucket, "test").await.unwrap();
    let (data, listed) = LocalData::open(&root.join("dirty"), &root.join("recent"), 8 * CHUNK, CHUNK).unwrap();
    Disk::open(DiskParts {
        disk_id: "test".into(),
        manifest,
        manifest_hash,
        profile: Vec::new(),
        data,
        listed,
        cache: Arc::new(ChunkCache::load(root.join("cache"), 16 * CHUNK, bucket.clone()).unwrap()),
        page_store: Arc::new(PageStore::new(root.join("pages"), bucket.clone()).unwrap()),
        bucket,
        node,
        dirty_limit_bytes: 2 * CHUNK,
        limits: Default::default(),
        host_pressure: Arc::new(AtomicBool::new(false)),
        checkpoint_after_bytes: CHUNK,
        max_unsaved_age: Duration::from_secs(60),
        wait_when_behind: false,
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn blocked_writers_wake_on_terminal_failures() {
    for ownership_lost in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let disk = open_disk(root.path()).await;
        disk.write(0, vec![1; (2 * CHUNK) as usize]).await.unwrap();
        let first = disk.write(2 * CHUNK, vec![2; CHUNK as usize]);
        let second = disk.write(3 * CHUNK, vec![3; CHUNK as usize]);
        tokio::pin!(first, second);
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        if ownership_lost {
            assert!(disk.lose_ownership("test").is_err());
        } else {
            disk.fail_sync(&anyhow::anyhow!("test"));
        }
        for writer in [first, second] {
            let result = tokio::time::timeout(Duration::from_secs(1), writer).await.unwrap();
            assert!(result.is_err());
        }
        assert_eq!(disk.local_chunks.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn saved_chunks_stay_local_up_to_the_cache_budget() {
    let root = tempfile::tempdir().unwrap();
    let disk = open_disk(root.path()).await;
    disk.write(0, vec![1; (2 * CHUNK) as usize]).await.unwrap();
    disk.checkpoint().await.unwrap();
    assert_eq!(disk.local_chunks.load(Ordering::SeqCst), 2);
    disk.cache.set_limit(CHUNK);
    disk.free_saved_chunks().await.unwrap();
    assert_eq!(disk.local_chunks.load(Ordering::SeqCst), 1);
    assert_eq!(disk.read(0, (2 * CHUNK) as usize).await.unwrap(), vec![1; (2 * CHUNK) as usize]);
}

#[tokio::test]
async fn a_detach_caches_only_the_newest_saved_chunks() {
    let root = tempfile::tempdir().unwrap();
    let disk = open_disk(root.path()).await;
    disk.write(0, vec![1; (2 * CHUNK) as usize]).await.unwrap();
    disk.checkpoint().await.unwrap();
    disk.cache.set_limit(CHUNK);
    disk.cache_local_chunks().await;
    let newest = disk.saved_local.lock().unwrap().back().copied().unwrap();
    let hash = disk.slot(newest).await.unwrap().lock().await.base.unwrap();
    assert!(disk.cache.has(&hash));
    assert_eq!(disk.cache.bytes(), CHUNK);
}

#[tokio::test]
async fn host_pressure_frees_saved_chunks() {
    let root = tempfile::tempdir().unwrap();
    let disk = open_disk(root.path()).await;
    disk.write(0, vec![1; CHUNK as usize]).await.unwrap();
    disk.checkpoint().await.unwrap();
    disk.host_pressure.store(true, Ordering::Release);
    disk.free_saved_chunks().await.unwrap();
    assert_eq!(disk.local_chunks.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn checkpoints_release_saved_local_chunks() {
    let root = tempfile::tempdir().unwrap();
    let disk = open_disk(root.path()).await;
    disk.cache.set_limit(0);
    for value in 1..10 {
        disk.write(0, vec![value; CHUNK as usize]).await.unwrap();
        disk.checkpoint().await.unwrap();
        assert!(disk.saved_local.lock().unwrap().is_empty());
        assert_eq!(disk.local_chunks.load(Ordering::SeqCst), 0);
    }
    disk.write(CHUNK, vec![10; (2 * CHUNK) as usize]).await.unwrap();
    disk.checkpoint().await.unwrap();
    assert!(!disk.slot(0).await.unwrap().lock().await.queued_for_eviction);
    disk.write(0, vec![11; CHUNK as usize]).await.unwrap();
    disk.checkpoint().await.unwrap();
    assert!(disk.saved_local.lock().unwrap().is_empty());
    assert_eq!(disk.read(0, CHUNK as usize).await.unwrap(), vec![11; CHUNK as usize]);
}

#[tokio::test]
async fn mixed_cold_local_and_zero_reads_preserve_offsets() {
    let root = tempfile::tempdir().unwrap();
    let disk = open_disk(root.path()).await;
    let mut expected: Vec<u8> = (1..=6).flat_map(|value| vec![value; CHUNK as usize]).collect();
    disk.write(0, expected.clone()).await.unwrap();
    disk.checkpoint().await.unwrap();
    disk.cache.prune(&Default::default());
    expected.extend(vec![0; (2 * CHUNK) as usize]);
    let start = 512;
    let end = expected.len() - 512;
    assert_eq!(disk.read(start as u64, end - start).await.unwrap(), expected[start..end]);
}

#[tokio::test]
async fn unchanged_chunks_skip_put_but_recovery_repairs_missing_objects() {
    let root = tempfile::tempdir().unwrap();
    let disk = open_disk(root.path()).await;
    let content = vec![7; CHUNK as usize];
    disk.write(0, content.clone()).await.unwrap();
    disk.checkpoint().await.unwrap();
    disk.write(0, content.clone()).await.unwrap();
    let before = disk.bucket.request_counts().put;
    disk.checkpoint().await.unwrap();
    // Page, manifest, and head only; no chunk PUT.
    assert_eq!(disk.bucket.request_counts().put - before, 3);
    let hash = ContentHash::of(&content);
    disk.bucket.delete(&keys::chunk(&hash)).await.unwrap();
    disk.write(0, content).await.unwrap();
    disk.slot(0).await.unwrap().lock().await.confirm = true;
    disk.checkpoint().await.unwrap();
    let packed = disk.bucket.get(&keys::chunk(&hash)).await.unwrap().unwrap();
    hash.verify(&crate::codec::unpack(&packed, CHUNK as usize).unwrap()).unwrap();
}

#[tokio::test]
async fn online_growth_preserves_pending_writes_and_exposes_zeroed_space() {
    let root = tempfile::tempdir().unwrap();
    let disk = open_disk(root.path()).await;
    disk.write(0, vec![17; CHUNK as usize]).await.unwrap();
    let size = (1 << 30) + CHUNK;
    assert_eq!(disk.grow_to(size).await.unwrap(), size);
    assert_eq!(disk.grow_to(size).await.unwrap(), size);
    assert!(disk.grow_to(size - CHUNK).await.is_err());
    assert_eq!(disk.read(0, CHUNK as usize).await.unwrap(), vec![17; CHUNK as usize]);
    assert_eq!(disk.read(size - CHUNK, CHUNK as usize).await.unwrap(), vec![0; CHUNK as usize]);
    disk.write(size - CHUNK, vec![23; CHUNK as usize]).await.unwrap();
    disk.flush().await.unwrap();
    disk.checkpoint().await.unwrap();
    assert_eq!(disk.read(size - CHUNK, CHUNK as usize).await.unwrap(), vec![23; CHUNK as usize]);
    let head = crate::records::Head::load(&disk.bucket, "test").await.unwrap().unwrap();
    assert_eq!(Manifest::load(&disk.bucket, &head.manifest_hash().unwrap()).await.unwrap().disk_size, size);
}

#[tokio::test]
async fn changing_limits_preserves_them_through_checkpoint_and_resize() {
    let root = tempfile::tempdir().unwrap();
    let disk = open_disk(root.path()).await;
    let limits = crate::disk_limits::DiskLimits { cache_bytes: Some(CHUNK), dirty_bytes: Some(2 * CHUNK) };
    disk.set_limits(limits, 2 * CHUNK).await.unwrap();
    disk.write(0, vec![9; (2 * CHUNK) as usize]).await.unwrap();
    let write = disk.write(2 * CHUNK, vec![4; CHUNK as usize]);
    tokio::pin!(write);
    assert!(futures::poll!(&mut write).is_pending());
    disk.checkpoint().await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), write).await.unwrap().unwrap();
    disk.grow_to(16 * CHUNK).await.unwrap();
    disk.checkpoint().await.unwrap();
    assert!(disk.cache.bytes() <= CHUNK);
    assert_eq!(disk.local_chunks.load(Ordering::SeqCst), 1);
    let head = crate::records::Head::load(&disk.bucket, "test").await.unwrap().unwrap();
    assert_eq!(head.limits.cache_bytes, Some(CHUNK));
    assert_eq!(head.limits.dirty_bytes, Some(2 * CHUNK));
}

#[tokio::test]
async fn reads_larger_than_the_cache_keep_downloaded_files_pinned() {
    let root = tempfile::tempdir().unwrap();
    let disk = open_disk(root.path()).await;
    let expected: Vec<u8> = (1..=6).flat_map(|byte| vec![byte; CHUNK as usize]).collect();
    disk.write(0, expected.clone()).await.unwrap();
    disk.checkpoint().await.unwrap();
    disk.cache.prune(&Default::default());
    disk.cache.set_limit(CHUNK);
    assert_eq!(disk.read(0, expected.len()).await.unwrap(), expected);
    assert!(disk.cache.bytes() <= CHUNK);
}
