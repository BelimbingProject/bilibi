//! Reading stored bytes back: signed single-asset links, and the two bounds
//! every read passes. A stream (the Web host's media routes) is sent as it is
//! read, never held in memory whole, and takes one of [`MAX_STREAMS`] slots or
//! is refused `Busy`. A read that needs the whole file in memory (an export,
//! a scan, an owner's own use) waits for one of [`MAX_LOADS`] slots.
use crate::{storage, Error, FileStream, Media, Result, Stored, StreamFile, EXTERNAL};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_postgres::GenericClient;

/// Files sent at once by the media routes. Each holds an open file and a
/// small buffer, no connection; one more is refused `Busy` (503).
pub(crate) const MAX_STREAMS: usize = 64;
/// Whole files in memory at once (each at most `media.max_bytes`); one more
/// waits its turn.
pub(crate) const MAX_LOADS: usize = 4;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
fn filename(row: &Stored) -> String {
    row.asset
        .original_filename
        .clone()
        .unwrap_or_else(|| row.key.rsplit('/').next().unwrap_or("download").into())
}

impl Media {
    /// The whole file in memory, after waiting for a load slot. A file whose
    /// bytes are gone is `StorageUnavailable`.
    pub(crate) async fn load(&self, roots: &Value, disk: &str, key: &str) -> Result<Vec<u8>> {
        let _slot = self
            .loads
            .acquire()
            .await
            .map_err(|_| Error::StorageUnavailable)?;
        storage::open(roots, disk, key)
            .await?
            .ok_or(Error::StorageUnavailable)?
            .read_all()
            .await
    }
    /// `row`'s file opened for sending, holding a stream slot: `Busy` when
    /// every slot is taken, `StorageUnavailable` when its bytes are gone.
    pub(crate) async fn stream(
        &self,
        client: &(impl GenericClient + Sync),
        row: &Stored,
    ) -> Result<FileStream> {
        let permit = self
            .streams
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Busy)?;
        let opened = storage::open(&self.disks(client).await?, &row.disk, &row.key)
            .await?
            .ok_or(Error::StorageUnavailable)?;
        Ok(FileStream {
            filename: filename(row),
            mime_type: row.asset.mime_type.clone(),
            len: opened.size(),
            reader: opened.into_reader(),
            permit,
        })
    }

    /// Safe HTTP(S) link for an `external` asset, else a signed stream path.
    pub async fn display_url(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &bilibi_tenancy::Scope,
        id: i64,
    ) -> Result<String> {
        let row = self.public(client, scope, id).await?;
        if row.disk == EXTERNAL {
            let url = row
                .asset
                .metadata
                .as_ref()
                .and_then(|m| m.get("public_url"))
                .and_then(Value::as_str);
            return url
                .filter(|url| safe_external_url(url))
                .map(str::to_owned)
                .ok_or(Error::InvalidExternalUrl);
        }
        self.stream_url(client, scope, id).await
    }
    /// Relative path with an expiring token. It authorizes this one public
    /// asset for its issued lifetime and is separate from private downloads.
    pub async fn stream_url(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &bilibi_tenancy::Scope,
        id: i64,
    ) -> Result<String> {
        self.tenant_stream_url(client, scope.tenant(), id).await
    }
    /// [`Media::stream_url`] for a page with no session, such as a portal
    /// sign-in card. The caller takes `tenant` from its own record (the portal
    /// site an address names), never from the request, and `id` from the same
    /// record; another tenant's asset is `NotFound`.
    pub async fn tenant_stream_url(
        &self,
        client: &(impl GenericClient + Sync),
        tenant: i64,
        id: i64,
    ) -> Result<String> {
        let row = self.fetch(client, tenant, id).await?;
        if self.private_tree(client, id).await? {
            return Err(Error::PrivateAsset);
        }
        if row.disk == EXTERNAL {
            return Err(Error::SourceNotStored);
        }
        self.readable(client, &row).await?;
        let ttl = self
            .setting(client, "media.stream_ttl_seconds")
            .await?
            .as_u64()
            .ok_or(Error::InvalidRules)?;
        let token = self.signer.sign(id, row.tenant, now() + ttl)?;
        Ok(format!("/media/assets/{id}/stream?token={token}"))
    }
    /// The checks of the unauthenticated edge. The token alone proves
    /// permission; the row, the tenant it carries and the private-tree rule
    /// are rechecked here.
    async fn signed(
        &self,
        client: &(impl GenericClient + Sync),
        id: i64,
        token: &str,
    ) -> Result<Stored> {
        let claims = self.signer.verify(id, token, now())?;
        let row = self.fetch(client, claims.tenant, id).await?;
        if self.private_tree(client, id).await? {
            return Err(Error::PrivateAsset);
        }
        if row.disk == EXTERNAL {
            return Err(Error::SourceNotStored);
        }
        // Rechecked at every read: a link issued while the file was clean
        // stops working if it is rescanned and quarantined.
        self.readable(client, &row).await?;
        Ok(row)
    }
    /// A signed link's bytes in memory, for an owner that needs them whole
    /// (an image in a PDF). Waits for a load slot.
    pub async fn read_signed_asset(
        &self,
        client: &(impl GenericClient + Sync),
        id: i64,
        token: &str,
    ) -> Result<StreamFile> {
        let row = self.signed(client, id, token).await?;
        let bytes = self
            .load(&self.disks(client).await?, &row.disk, &row.key)
            .await?;
        Ok(StreamFile {
            bytes,
            filename: filename(&row),
            mime_type: row.asset.mime_type,
        })
    }
    /// A signed link's file opened for the Web host's stream route: the same
    /// checks as [`Media::read_signed_asset`], sent as it is read. Drop the
    /// connection before sending; `Busy` when every stream slot is taken.
    pub async fn open_signed_asset(
        &self,
        client: &(impl GenericClient + Sync),
        id: i64,
        token: &str,
    ) -> Result<FileStream> {
        let row = self.signed(client, id, token).await?;
        self.stream(client, &row).await
    }
}

fn safe_external_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let Some(rest) = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    url.len() <= 2048
        && !authority.is_empty()
        && !authority.contains('@')
        && url.bytes().all(|b| b.is_ascii_graphic())
}
