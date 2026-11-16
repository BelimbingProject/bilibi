//! Disk reconciliation for an operator (issue 599). Files and rows cannot
//! share a rollback, so a crash between them, a removal that failed after its
//! rows committed (`CleanupFailed`) or a restored backup can part them. This
//! lists both sides and changes nothing: the operator decides what to remove
//! or restore.
use crate::{storage, Media, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use tokio::fs;
use tokio_postgres::GenericClient;

/// A file younger than this may still be on its way to its row (bytes are
/// written first), so it is never listed as unneeded.
const SETTLING: Duration = Duration::from_secs(3600);
/// Keys checked against the database, or rows checked against the disk, at once.
const BATCH: usize = 1_000;
/// Resumable-upload staging files are transport with their own sweep, never rows.
const STAGING: &str = "staging";

/// A stored asset whose file is not on its disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingFile {
    pub id: i64,
    pub tenant: i64,
    pub disk: String,
    pub key: String,
}

/// What [`Media::reconcile`] found.
#[derive(Clone, Debug, Default)]
pub struct Reconciliation {
    /// Disk and key of each file, older than an hour, that no row needs: no
    /// asset names it (a purged asset no longer does) and no erased
    /// attachment still waits for its bytes to go.
    pub unneeded_files: Vec<(String, String)>,
    /// Assets that are neither purged nor being purged whose file is gone.
    pub missing_files: Vec<MissingFile>,
    /// Disks that rows name or `media.disks` maps but that cannot be read
    /// safely (not mapped, not `0700`, a symlink in the path): nothing on
    /// them was checked.
    pub unreadable_disks: BTreeSet<String>,
}

impl Media {
    /// Lists files on the mapped disks that no row needs and stored assets
    /// whose file is gone, in every tenant. Read-only; for the operator's
    /// `media-reconcile` host command, which has no session.
    pub async fn reconcile(&self, client: &(impl GenericClient + Sync)) -> Result<Reconciliation> {
        let mut found = Reconciliation::default();
        let mut readable = BTreeMap::new();
        let disks = self.disks(client).await?;
        for (disk, root) in disks.as_object().into_iter().flatten() {
            match root.as_str() {
                Some(root) if storage::validate_root(root).await.is_ok() => {
                    readable.insert(disk.clone(), PathBuf::from(root));
                }
                _ => {
                    found.unreadable_disks.insert(disk.clone());
                }
            }
        }
        for (disk, root) in &readable {
            let mut keys = Vec::new();
            let mut pending = vec![root.clone()];
            while let Some(directory) = pending.pop() {
                let Ok(mut entries) = fs::read_dir(&directory).await else {
                    found.unreadable_disks.insert(disk.clone());
                    continue;
                };
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let path = entry.path();
                    // Never follow a link out of the root.
                    let Ok(stat) = fs::symlink_metadata(&path).await else {
                        continue;
                    };
                    if stat.is_dir() {
                        if !(directory == *root && entry.file_name() == STAGING) {
                            pending.push(path);
                        }
                    } else if stat.modified().is_ok_and(|at| {
                        SystemTime::now()
                            .duration_since(at)
                            .is_ok_and(|age| age >= SETTLING)
                    }) {
                        keys.push(key_of(root, &path));
                        if keys.len() == BATCH {
                            found
                                .unneeded_files
                                .extend(unneeded(client, disk, &keys).await?);
                            keys.clear();
                        }
                    }
                }
            }
            found
                .unneeded_files
                .extend(unneeded(client, disk, &keys).await?);
        }
        let mut after = 0_i64;
        loop {
            let rows = client
                .query(
                    "SELECT id,tenant_id,disk,storage_key FROM base_media_assets WHERE disk<>'external' \
                     AND purge_requested_at IS NULL AND bytes_purged_at IS NULL AND id>$1 ORDER BY id LIMIT $2",
                    &[&after, &(BATCH as i64)],
                )
                .await?;
            let Some(last) = rows.last() else { break };
            after = last.get(0);
            for row in &rows {
                let (disk, key): (String, String) = (row.get(2), row.get(3));
                let Some(root) = readable.get(&disk) else {
                    found.unreadable_disks.insert(disk);
                    continue;
                };
                let present = storage::validate_key(&key).is_ok()
                    && fs::symlink_metadata(root.join(&key))
                        .await
                        .is_ok_and(|stat| stat.is_file());
                if !present {
                    found.missing_files.push(MissingFile {
                        id: row.get(0),
                        tenant: row.get(1),
                        disk,
                        key,
                    });
                }
            }
        }
        Ok(found)
    }
}

fn key_of(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// The keys among `keys` on `disk` that no live asset or erased attachment
/// still waiting for its bytes to go names.
async fn unneeded(
    client: &(impl GenericClient + Sync),
    disk: &str,
    keys: &[String],
) -> Result<Vec<(String, String)>> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let rows = client
        .query(
            "SELECT k FROM unnest($2::text[]) AS k WHERE NOT EXISTS (SELECT 1 FROM base_media_assets \
             WHERE disk<>'external' AND disk=$1 AND storage_key=k AND bytes_purged_at IS NULL) \
             AND NOT EXISTS (SELECT 1 FROM base_media_attachments WHERE state='erased' \
             AND bytes_removed_at IS NULL AND erased_disk=$1 AND erased_key=k) ORDER BY k",
            &[&disk, &keys],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| (disk.to_owned(), row.get::<_, String>(0)))
        .collect())
}
