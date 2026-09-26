use crate::bucket::{Bucket, keys};
use crate::content_hash::ContentHash;
use crate::manifest::Manifest;
use crate::records::{AttachedMarker, Head, Snapshot};
use anyhow::{Result, anyhow, bail, ensure};

/// Disk operations that only touch S3: create, clone, snapshot, resize and load.
pub struct Catalog<'a> {
    bucket: &'a Bucket,
    node_name: &'a str,
}

#[derive(Debug, serde::Serialize)]
pub struct DiskSummary {
    pub disk: String,
    pub size: u64,
    pub seq: u64,
    pub time: String,
    pub attached_on: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct SnapshotSummary {
    pub name: String,
    pub size: u64,
    pub disk: Option<String>,
    pub time: Option<String>,
}

pub struct LoadedDisk {
    pub head: Head,
    pub manifest: Manifest,
    pub manifest_hash: ContentHash,
}

impl<'a> Catalog<'a> {
    pub fn new(bucket: &'a Bucket, node_name: &'a str) -> Self {
        Self { bucket, node_name }
    }

    pub async fn create(&self, disk_id: &str, size: u64) -> Result<()> {
        validate_name(disk_id)?;
        ensure!(size > 0, "disk size must be more than zero");
        self.ensure_new_disk(disk_id).await?;
        let manifest = Manifest::empty(disk_id, size);
        let hash = manifest.save(self.bucket).await?;
        Head::new(&hash, Vec::new(), 0, self.node_name).save(self.bucket, disk_id).await
    }

    /// A clone points to the manifest of the snapshot. It copies no chunks.
    pub async fn clone_snapshot(&self, snapshot: &str, disk_id: &str, size: Option<u64>) -> Result<()> {
        validate_name(disk_id)?;
        self.ensure_new_disk(disk_id).await?;
        let record = Snapshot::load(self.bucket, snapshot)
            .await?
            .ok_or_else(|| anyhow!("snapshot {snapshot} does not exist"))?;
        let mut hash = ContentHash::from_reference(&record.manifest)?;
        let mut manifest = Manifest::load(self.bucket, &hash).await?;
        if let Some(size) = size.filter(|size| *size > manifest.disk_size) {
            manifest = manifest.next(disk_id, hash);
            manifest.grow_to(size)?;
            hash = manifest.save(self.bucket).await?;
        }
        Head::new(&hash, record.profile, manifest.seq, self.node_name).save(self.bucket, disk_id).await?;
        // If the snapshot was deleted meanwhile, GC may free its chunks. Undo.
        if !self.bucket.exists(&keys::snapshot(snapshot)).await? {
            self.bucket.delete(&keys::head(disk_id)).await?;
            bail!("snapshot {snapshot} was deleted while the disk was being created");
        }
        Ok(())
    }

    /// Records the current head of a disk under a name.
    pub async fn snapshot(&self, disk_id: &str, name: &str) -> Result<()> {
        validate_name(name)?;
        if self.bucket.exists(&keys::snapshot(name)).await? {
            bail!("snapshot {name} already exists");
        }
        let head = self.load_head(disk_id).await?;
        Snapshot {
            manifest: head.manifest,
            profile: head.profile,
            disk: Some(disk_id.to_string()),
            time: Some(crate::records::now_text()),
        }
        .save(self.bucket, name)
        .await
    }

    /// Deletes only the head. Chunks and manifests stay, because snapshots and
    /// disks made from them may share them.
    pub async fn delete_disk(&self, disk_id: &str) -> Result<()> {
        self.load_head(disk_id).await?;
        if let Some(marker) = AttachedMarker::load(self.bucket, disk_id).await? {
            bail!("disk {disk_id} is attached on {}. Detach it first", marker.node);
        }
        self.bucket.delete(&keys::head(disk_id)).await
    }

    /// Disks made from the snapshot keep working: they point to its manifest.
    pub async fn delete_snapshot(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        if !self.bucket.exists(&keys::snapshot(name)).await? {
            bail!("snapshot {name} does not exist");
        }
        self.bucket.delete(&keys::snapshot(name)).await
    }

    pub async fn list_snapshots(&self) -> Result<Vec<String>> {
        let mut names = self.bucket.list_objects("snapshots").await?;
        names.sort();
        Ok(names)
    }

    /// Two small reads: the record, and the first bytes of its manifest.
    pub async fn snapshot_summary(&self, name: &str) -> Result<SnapshotSummary> {
        let record = Snapshot::load(self.bucket, name).await?.ok_or_else(|| anyhow!("snapshot {name} does not exist"))?;
        let size = self.manifest_disk_size(&ContentHash::from_reference(&record.manifest)?).await?;
        Ok(SnapshotSummary { name: name.to_string(), size, disk: record.disk, time: record.time })
    }

    /// Makes a detached disk larger. Grow the filesystem in the guest after this.
    pub async fn resize(&self, disk_id: &str, size: u64) -> Result<()> {
        if let Some(marker) = AttachedMarker::load(self.bucket, disk_id).await? {
            bail!("disk {disk_id} is attached on {}. Detach it first", marker.node);
        }
        let disk = self.load(disk_id).await?;
        let mut manifest = disk.manifest.next(disk_id, disk.manifest_hash);
        manifest.grow_to(size)?;
        let hash = manifest.save(self.bucket).await?;
        Head::new(&hash, disk.head.profile, manifest.seq, self.node_name).save(self.bucket, disk_id).await
    }

    pub async fn list_disks(&self) -> Result<Vec<String>> {
        let mut disks = self.bucket.list_folders("disks").await?;
        disks.sort();
        Ok(disks)
    }

    /// Three small reads: the head, the marker, and the first bytes of the
    /// manifest, which hold the disk size. The full manifest is not read.
    pub async fn summary(&self, disk_id: &str) -> Result<DiskSummary> {
        let head = self.load_head(disk_id).await?;
        let marker = AttachedMarker::load(self.bucket, disk_id).await?;
        let size = self.manifest_disk_size(&head.manifest_hash()?).await?;
        Ok(DiskSummary {
            disk: disk_id.to_string(),
            size,
            seq: head.seq,
            time: head.time,
            attached_on: marker.map(|marker| marker.node),
        })
    }

    pub async fn load(&self, disk_id: &str) -> Result<LoadedDisk> {
        let head = self.load_head(disk_id).await?;
        let manifest_hash = head.manifest_hash()?;
        let manifest = Manifest::load(self.bucket, &manifest_hash).await?;
        Ok(LoadedDisk { head, manifest, manifest_hash })
    }

    /// The disk size is at bytes 12..20 of a manifest. See `manifest.rs`.
    async fn manifest_disk_size(&self, hash: &ContentHash) -> Result<u64> {
        let header = self.bucket.get_range(&keys::manifest(hash), 12..20).await?;
        Ok(u64::from_le_bytes(header.as_ref().try_into().map_err(|_| anyhow!("manifest header is too short"))?))
    }

    async fn load_head(&self, disk_id: &str) -> Result<Head> {
        validate_name(disk_id)?;
        Head::load(self.bucket, disk_id).await?.ok_or_else(|| anyhow!("disk {disk_id} does not exist"))
    }

    async fn ensure_new_disk(&self, disk_id: &str) -> Result<()> {
        if self.bucket.exists(&keys::head(disk_id)).await? {
            bail!("disk {disk_id} already exists");
        }
        Ok(())
    }
}

/// Disk IDs and snapshot names become S3 keys and file names.
pub fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    ensure!(valid, "bad name {name:?}: use letters, digits, '-', '_' and '.'");
    Ok(())
}
