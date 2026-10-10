//! Microsoft Graph client for one drive: retries, throttling, auth refresh,
//! streaming transfers with hashing and rate limiting.

use crate::auth::TokenManager;
use crate::error::{GraphError, Result};
use crate::model::*;
use crate::ratelimit::RateLimiter;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use od_core::quickxor::QuickXorHash;
use reqwest::{RequestBuilder, Response, StatusCode, header};
use serde::de::DeserializeOwned;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

pub const DEFAULT_GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

/// Fragment size for upload sessions: 10 MiB, a multiple of 320 KiB as required.
pub const UPLOAD_FRAGMENT: usize = 32 * 320 * 1024;

/// Files at most this size may use a single PUT. We still prefer upload
/// sessions for non-empty files because they carry `fileSystemInfo`.
pub const SIMPLE_UPLOAD_MAX: u64 = 4 * 1024 * 1024;

/// Undocumented but required since 2025-03 so that folders added to
/// "My files" keep appearing in delta/children results (research doc 02 §3.1).
pub const PREFER_ADD_TO_ONEDRIVE: &str = "Include-Feature=AddToOneDrive";

/// Properties requested on the initial delta (nextLink/deltaLink preserve it).
pub const DELTA_SELECT: &str = "id,name,eTag,cTag,deleted,file,folder,root,fileSystemInfo,remoteItem,\
parentReference,size,createdBy,lastModifiedBy,package,lastModifiedDateTime,webUrl,shared,malware";

/// Personal drive ids are 16 hex digits but arrive with varying case and
/// sometimes without a leading zero; normalise before comparing or storing.
/// Business/SharePoint ids (`b!…`) are case-sensitive and returned unchanged.
pub fn normalize_drive_id(id: &str) -> String {
    if !id.is_empty() && id.len() <= 16 && id.chars().all(|c| c.is_ascii_hexdigit()) {
        format!("{:0>16}", id.to_ascii_lowercase())
    } else {
        id.to_string()
    }
}

/// How to resolve a name collision when creating an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictBehavior {
    Fail,
    Replace,
    Rename,
}

impl ConflictBehavior {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictBehavior::Fail => "fail",
            ConflictBehavior::Replace => "replace",
            ConflictBehavior::Rename => "rename",
        }
    }
}

/// Where an upload goes.
#[derive(Debug, Clone)]
pub enum UploadTarget {
    /// A new file named `name` in folder `parent_id`.
    New { parent_id: String, name: String },
    /// New content for an existing item; `if_match` guards against overwriting
    /// a newer remote version.
    Existing { item_id: String, if_match: Option<String> },
}

/// Result of sending one upload-session fragment.
#[derive(Debug)]
pub enum FragmentResult {
    /// More data expected, starting at this offset.
    Continue(u64),
    /// Upload finished.
    Complete(Box<DriveItem>),
}

/// Fields for `PATCH /items/{id}` (rename, move, set modification time).
#[derive(Debug, Clone, Default)]
pub struct ItemPatch {
    pub name: Option<String>,
    pub parent_id: Option<String>,
    pub modified: Option<DateTime<Utc>>,
    pub created: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub struct GraphClient {
    http: reqwest::Client,
    base: String,
    drive_prefix: String,
    /// The synced folder: the drive root, or `/items/{id}` for a shared folder.
    root_path: String,
    /// `Prefer` header sent on delta/children calls (see research doc 02 §3.1).
    prefer: Option<&'static str>,
    tokens: Arc<TokenManager>,
    pub upload_limiter: Arc<RateLimiter>,
    pub download_limiter: Arc<RateLimiter>,
    max_attempts: u32,
}

pub fn user_agent() -> String {
    // Decorated per Microsoft's throttling guidance: ISV|Company|App/Version.
    format!("ISV|LinuxOneDriveClient|LinuxOneDrive/{}", od_core::VERSION)
}

/// Shared HTTP client configuration.
pub fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(user_agent())
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(120))
        .pool_idle_timeout(Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .expect("http client")
}

pub fn graph_base_from_env() -> String {
    std::env::var("LINUX_ONEDRIVE_GRAPH_URL").unwrap_or_else(|_| DEFAULT_GRAPH_BASE.to_string())
}

impl GraphClient {
    /// Client for the signed-in user's default drive (`/me/drive`).
    pub fn new(http: reqwest::Client, base: impl Into<String>, tokens: Arc<TokenManager>) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
            drive_prefix: "/me/drive".into(),
            root_path: "/root".into(),
            prefer: Some(PREFER_ADD_TO_ONEDRIVE),
            tokens,
            upload_limiter: Arc::new(RateLimiter::unlimited()),
            download_limiter: Arc::new(RateLimiter::unlimited()),
            max_attempts: 6,
        }
    }

    /// Address a specific drive (`/drives/{id}`) instead of `/me/drive`.
    pub fn with_drive(mut self, drive_id: &str) -> Self {
        self.drive_prefix = format!("/drives/{}", normalize_drive_id(drive_id));
        self
    }

    /// Sync a folder of the drive instead of its root (a shortcut to a
    /// shared folder): `root()`, delta and notifications use it.
    pub fn with_root(mut self, item_id: &str) -> Self {
        self.root_path = format!("/items/{item_id}");
        self
    }

    /// The scoped root item id, if any (see [`Self::with_root`]).
    pub fn scoped_root(&self) -> Option<&str> {
        self.root_path.strip_prefix("/items/")
    }

    pub fn with_max_attempts(mut self, n: u32) -> Self {
        self.max_attempts = n.max(1);
        self
    }

    pub fn tokens(&self) -> &Arc<TokenManager> {
        &self.tokens
    }

    pub(crate) fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else {
            format!("{}{}", self.base, path)
        }
    }

    pub(crate) fn drive_url(&self, rest: &str) -> String {
        format!("{}{}{}", self.base, self.drive_prefix, rest)
    }

    /// Same as `drive_url` on the beta endpoint (for features only documented there).
    pub(crate) fn beta_drive_url(&self, rest: &str) -> String {
        let base = match self.base.strip_suffix("/v1.0") {
            Some(b) => format!("{b}/beta"),
            None => self.base.clone(),
        };
        format!("{}{}{}", base, self.drive_prefix, rest)
    }

    /// `…/items/{parent}:/{name}:{suffix}` with the name percent-encoded.
    fn child_path_url(&self, parent_id: &str, name: &str, suffix: &str) -> String {
        format!("{}{}/items/{}:/{}:{}", self.base, self.drive_prefix, parent_id, encode_segment(name), suffix)
    }

    /// Send an authenticated request with retries for transient failures,
    /// honouring `Retry-After`, and one token refresh on 401.
    pub(crate) async fn execute<F>(&self, make: F) -> Result<Response>
    where
        F: Fn(&reqwest::Client) -> RequestBuilder,
    {
        let mut attempt = 0u32;
        let mut refreshed = false;
        loop {
            attempt += 1;
            let token = self.tokens.access_token().await?;
            let result = make(&self.http).bearer_auth(&token).send().await;
            let err = match result {
                Ok(resp) if resp.status().is_success() || resp.status().is_redirection() => return Ok(resp),
                Ok(resp) if resp.status() == StatusCode::UNAUTHORIZED && !refreshed => {
                    refreshed = true;
                    self.tokens.force_refresh(&token).await?;
                    attempt -= 1;
                    continue;
                }
                Ok(resp) => error_from_response(resp).await,
                Err(e) => GraphError::Network(e),
            };
            if !err.is_transient() || attempt >= self.max_attempts {
                return Err(err);
            }
            let delay = err.retry_after().unwrap_or_else(|| backoff(attempt));
            if matches!(err.status(), Some(429 | 503 | 509)) {
                // Throttling is worth seeing without debug logs: it explains slow syncs.
                tracing::info!("OneDrive is limiting requests; trying again in {} s", delay.as_secs().max(1));
            } else {
                tracing::debug!(attempt, ?delay, error = %err, "retrying Graph request");
            }
            tokio::time::sleep(delay).await;
        }
    }

    pub(crate) async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self.execute(|c| c.get(url)).await?;
        parse_json(resp).await
    }

    /// GET with the `Prefer` header used for enumeration calls.
    async fn get_json_prefer<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let prefer = self.prefer;
        let resp = self
            .execute(|c| {
                let rb = c.get(url);
                match prefer {
                    Some(p) => rb.header("Prefer", p),
                    None => rb,
                }
            })
            .await?;
        parse_json(resp).await
    }

    // ---------------------------------------------------------------- identity

    pub async fn me(&self) -> Result<User> {
        self.get_json(&self.url("/me")).await
    }

    /// Organization display name for work/school accounts (None for personal).
    pub async fn organization_name(&self) -> Result<Option<String>> {
        let list: ValueList<Organization> = self.get_json(&self.url("/organization")).await?;
        Ok(list.value.into_iter().next().and_then(|o| o.display_name))
    }

    pub async fn drive(&self) -> Result<Drive> {
        self.get_json(&self.drive_url("")).await
    }

    // ---------------------------------------------------------------- reading

    pub async fn root(&self) -> Result<DriveItem> {
        self.get_json(&self.drive_url(&self.root_path)).await
    }

    pub async fn item(&self, item_id: &str) -> Result<DriveItem> {
        self.get_json(&self.drive_url(&format!("/items/{item_id}"))).await
    }

    /// Look up a child by name (case-insensitive on the server).
    pub async fn child_by_name(&self, parent_id: &str, name: &str) -> Result<DriveItem> {
        self.get_json(&self.child_path_url(parent_id, name, "")).await
    }

    /// All children of a folder (follows paging).
    pub async fn children(&self, item_id: &str) -> Result<Vec<DriveItem>> {
        let mut url = self.drive_url(&format!("/items/{item_id}/children?$top=1000"));
        let mut out = Vec::new();
        loop {
            let page: ItemPage = self.get_json_prefer(&url).await?;
            out.extend(page.value);
            match page.next_link {
                Some(next) => url = next,
                None => return Ok(out),
            }
        }
    }

    /// Children of an item living in another drive (shared folder shortcuts).
    pub async fn remote_children(&self, drive_id: &str, item_id: &str) -> Result<Vec<DriveItem>> {
        let mut url = self.url(&format!("/drives/{drive_id}/items/{item_id}/children?$top=1000"));
        let mut out = Vec::new();
        loop {
            let page: ItemPage = self.get_json_prefer(&url).await?;
            out.extend(page.value);
            match page.next_link {
                Some(next) => url = next,
                None => return Ok(out),
            }
        }
    }

    /// Everything under the synced root, root first, parents before children:
    /// a full listing for drives where delta isn't available on folders
    /// (shared folders on work or school accounts). Fails as a whole if any
    /// folder can't be listed, so a partial listing is never mistaken for
    /// deletions.
    pub async fn walk(&self) -> Result<Vec<DriveItem>> {
        use futures::StreamExt;
        let root = self.root().await?;
        let mut out = vec![root.clone()];
        let mut level = vec![root.id.clone()];
        while !level.is_empty() {
            let pages: Vec<Result<Vec<DriveItem>>> = futures::stream::iter(level.into_iter().map(|id| {
                let me = self.clone();
                async move { me.children(&id).await }
            }))
            .buffered(4)
            .collect()
            .await;
            let mut next = Vec::new();
            for page in pages {
                for it in page? {
                    if it.is_folder() && it.remote_item.is_none() && it.package.is_none() {
                        next.push(it.id.clone());
                    }
                    out.push(it);
                }
            }
            level = next;
        }
        Ok(out)
    }

    /// First delta request for the whole drive (`link == None`) or the next
    /// page / next round (`link` = a nextLink or deltaLink).
    pub async fn delta_page(&self, link: Option<&str>) -> Result<ItemPage> {
        let url = match link {
            Some(l) => self.url(l),
            None => self.drive_url(&format!("{}/delta?$select={DELTA_SELECT}", self.root_path)),
        };
        self.get_json_prefer(&url).await
    }

    /// A deltaLink representing "now", without enumerating existing items.
    pub async fn latest_delta_link(&self) -> Result<String> {
        let page: ItemPage =
            self.get_json_prefer(&self.drive_url(&format!("{}/delta?token=latest", self.root_path))).await?;
        page.delta_link.ok_or_else(|| GraphError::InvalidResponse("no deltaLink for token=latest".into()))
    }

    // ---------------------------------------------------------------- downloads

    /// Stream an item's content into `dest`.
    ///
    /// If `resume` and `dest` already holds a prefix, only the remainder is
    /// requested (HTTP Range). The QuickXorHash of the complete file is
    /// computed on the fly and compared with `expected_hash` when given; the
    /// file is fsynced before returning. Returns the computed hash.
    pub async fn download_to(
        &self,
        item_id: &str,
        dest: &Path,
        expected_size: Option<u64>,
        expected_hash: Option<&str>,
        resume: bool,
        cancel: &CancellationToken,
        progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<String> {
        let url = self.drive_url(&format!("/items/{item_id}/content"));
        self.download_url_to(&url, true, dest, expected_size, expected_hash, resume, cancel, progress).await
    }

    /// A fresh pre-authenticated download URL for an item (valid for minutes).
    pub async fn download_url(&self, item_id: &str) -> Result<String> {
        let it: DriveItem = self
            .get_json(&self.drive_url(&format!("/items/{item_id}?$select=id,@microsoft.graph.downloadUrl")))
            .await?;
        it.download_url.ok_or_else(|| GraphError::InvalidResponse("no download URL for this item".into()))
    }

    /// Bytes `[start, end]` (inclusive) from a pre-authenticated download URL,
    /// rate-limited. Fails unless exactly that range comes back.
    pub async fn fetch_range(&self, url: &str, start: u64, end: u64) -> Result<Bytes> {
        let want = (end - start + 1) as usize;
        self.download_limiter.acquire(want).await;
        let resp = self.http.get(url).header(header::RANGE, format!("bytes={start}-{end}")).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(error_from_response(resp).await);
        }
        let body = resp.bytes().await?;
        if status == StatusCode::PARTIAL_CONTENT && body.len() == want {
            Ok(body)
        } else if status == StatusCode::OK && start == 0 && body.len() >= want {
            Ok(body.slice(..want))
        } else {
            Err(GraphError::InvalidResponse(format!(
                "expected {want} bytes from {start}, got {} ({status})",
                body.len()
            )))
        }
    }

    /// The first bytes of a file (`[start, start+len)`), without downloading
    /// the rest even if the server ignores the Range header.
    pub async fn download_range(&self, item_id: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let url = self.drive_url(&format!("/items/{item_id}/content"));
        let end = start + len - 1;
        let resp = self.execute(|c| c.get(&url).header(header::RANGE, format!("bytes={start}-{end}"))).await?;
        let skip = if resp.status() == StatusCode::PARTIAL_CONTENT { 0 } else { start as usize };
        let mut out = Vec::with_capacity(len as usize);
        let mut seen = 0usize;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            let mut slice = &chunk[..];
            if seen < skip {
                let s = (skip - seen).min(slice.len());
                seen += s;
                slice = &slice[s..];
            }
            let want = len as usize - out.len();
            out.extend_from_slice(&slice[..slice.len().min(want)]);
            if out.len() >= len as usize {
                break;
            }
        }
        Ok(out)
    }

    /// Download a specific historical version of a file.
    pub async fn download_version_to(
        &self,
        item_id: &str,
        version_id: &str,
        dest: &Path,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let url = self.drive_url(&format!("/items/{item_id}/versions/{version_id}/content"));
        self.download_url_to(&url, true, dest, None, None, false, cancel, &|_| {}).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_url_to(
        &self,
        url: &str,
        authenticated: bool,
        dest: &Path,
        expected_size: Option<u64>,
        expected_hash: Option<&str>,
        resume: bool,
        cancel: &CancellationToken,
        progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<String> {
        let mut hasher = QuickXorHash::new();
        let mut file =
            tokio::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dest).await?;
        let mut offset = file.metadata().await?.len();
        if !resume || expected_size.is_some_and(|s| offset > s) {
            file.set_len(0).await?;
            offset = 0;
        }
        if offset > 0 {
            // Re-hash the prefix we already have, streaming it from disk.
            use tokio::io::AsyncReadExt;
            let mut reader = tokio::fs::File::open(dest).await?;
            let mut buf = vec![0u8; 1 << 20];
            let mut remaining = offset;
            while remaining > 0 {
                let want = buf.len().min(remaining as usize);
                let n = reader.read(&mut buf[..want]).await?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                remaining -= n as u64;
            }
        }

        let resp = if authenticated {
            self.execute(|c| {
                let rb = c.get(url);
                if offset > 0 { rb.header(header::RANGE, format!("bytes={offset}-")) } else { rb }
            })
            .await?
        } else {
            let rb = self.http.get(url);
            let rb = if offset > 0 { rb.header(header::RANGE, format!("bytes={offset}-")) } else { rb };
            let r = rb.send().await?;
            if !r.status().is_success() {
                return Err(error_from_response(r).await);
            }
            r
        };

        if offset > 0 && resp.status() != StatusCode::PARTIAL_CONTENT {
            // Server ignored the range: start over.
            file.set_len(0).await?;
            hasher = QuickXorHash::new();
            offset = 0;
        }
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        progress(offset);

        let mut stream = resp.bytes_stream();
        let mut written = offset;
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(GraphError::Cancelled),
                c = stream.next() => c,
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk?;
            self.download_limiter.acquire(chunk.len()).await;
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
            progress(written);
        }
        file.flush().await?;
        file.sync_all().await?;

        if let Some(size) = expected_size
            && written != size
        {
            return Err(GraphError::InvalidResponse(format!("downloaded {written} bytes, expected {size}")));
        }
        let hash = hasher.finalize_base64();
        if let Some(expected) = expected_hash
            && !expected.is_empty()
            && expected != hash
        {
            // Corrupt or stale partial data: discard it so a retry starts clean.
            let _ = file.set_len(0).await;
            return Err(GraphError::HashMismatch);
        }
        Ok(hash)
    }

    // ---------------------------------------------------------------- uploads

    /// Upload with a single PUT (only for small files; used for empty files,
    /// which upload sessions do not accept).
    pub async fn upload_small(
        &self,
        target: &UploadTarget,
        data: Bytes,
        conflict: ConflictBehavior,
    ) -> Result<DriveItem> {
        let (url, if_match) = match target {
            UploadTarget::New { parent_id, name } => (
                format!(
                    "{}?@microsoft.graph.conflictBehavior={}",
                    self.child_path_url(parent_id, name, "/content"),
                    conflict.as_str()
                ),
                None,
            ),
            UploadTarget::Existing { item_id, if_match } => {
                (self.drive_url(&format!("/items/{item_id}/content")), if_match.clone())
            }
        };
        self.upload_limiter.acquire(data.len()).await;
        let resp = self
            .execute(|c| {
                let mut rb = c.put(&url).header(header::CONTENT_TYPE, "application/octet-stream").body(data.clone());
                if let Some(tag) = &if_match {
                    rb = rb.header(header::IF_MATCH, tag);
                }
                rb
            })
            .await?;
        parse_json(resp).await
    }

    /// Create a resumable upload session.
    ///
    /// `If-Match` is deliberately NOT sent for sessions: OneDrive changes the
    /// item's eTag after the session is created, which makes the final
    /// fragment fail with 412/416 (research doc 02 §6.2). Callers check the
    /// remote cTag immediately before creating the session instead.
    pub async fn create_upload_session(
        &self,
        target: &UploadTarget,
        modified: Option<DateTime<Utc>>,
        created: Option<DateTime<Utc>>,
        conflict: ConflictBehavior,
    ) -> Result<UploadSession> {
        let mut item = json!({ "@microsoft.graph.conflictBehavior": conflict.as_str() });
        let fsi = FileSystemInfo { created_date_time: created, last_modified_date_time: modified };
        if fsi.created_date_time.is_some() || fsi.last_modified_date_time.is_some() {
            item["fileSystemInfo"] = serde_json::to_value(&fsi).unwrap_or_default();
        }
        let url = match target {
            UploadTarget::New { parent_id, name } => self.child_path_url(parent_id, name, "/createUploadSession"),
            UploadTarget::Existing { item_id, .. } => self.drive_url(&format!("/items/{item_id}/createUploadSession")),
        };
        let body = json!({ "item": item });
        let resp = self.execute(|c| c.post(&url).json(&body)).await?;
        parse_json(resp).await
    }

    /// Send bytes `[start, start+data.len())` of a `total`-byte file.
    /// The upload URL is pre-authenticated: no Authorization header is sent.
    pub async fn upload_fragment(
        &self,
        upload_url: &str,
        data: Bytes,
        start: u64,
        total: u64,
    ) -> Result<FragmentResult> {
        let end = start + data.len() as u64 - 1;
        self.upload_limiter.acquire(data.len()).await;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let r = self
                .http
                .put(upload_url)
                .header(header::CONTENT_LENGTH, data.len())
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{total}"))
                .body(data.clone())
                .send()
                .await;
            let err = match r {
                Ok(resp) if resp.status() == StatusCode::ACCEPTED => {
                    let s: UploadSession = parse_json(resp).await?;
                    return Ok(FragmentResult::Continue(s.next_offset().unwrap_or(end + 1)));
                }
                Ok(resp) if resp.status().is_success() => {
                    let item: DriveItem = parse_json(resp).await?;
                    return Ok(FragmentResult::Complete(Box::new(item)));
                }
                Ok(resp) => error_from_response(resp).await,
                Err(e) => GraphError::Network(e),
            };
            if !err.is_transient() || attempt >= self.max_attempts {
                return Err(err);
            }
            tokio::time::sleep(err.retry_after().unwrap_or_else(|| backoff(attempt))).await;
        }
    }

    /// Query a session (to resume after a restart).
    pub async fn upload_session_status(&self, upload_url: &str) -> Result<UploadSession> {
        let resp = self.http.get(upload_url).send().await?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp).await);
        }
        parse_json(resp).await
    }

    pub async fn cancel_upload_session(&self, upload_url: &str) -> Result<()> {
        let _ = self.http.delete(upload_url).send().await?;
        Ok(())
    }

    // ---------------------------------------------------------------- mutations

    pub async fn create_folder(&self, parent_id: &str, name: &str, conflict: ConflictBehavior) -> Result<DriveItem> {
        let url = self.drive_url(&format!("/items/{parent_id}/children"));
        let body = json!({
            "name": name,
            "folder": {},
            "@microsoft.graph.conflictBehavior": conflict.as_str(),
        });
        let resp = self.execute(|c| c.post(&url).json(&body)).await?;
        parse_json(resp).await
    }

    pub async fn update_item(&self, item_id: &str, patch: &ItemPatch, if_match: Option<&str>) -> Result<DriveItem> {
        let url = self.drive_url(&format!("/items/{item_id}"));
        let mut body = serde_json::Map::new();
        if let Some(n) = &patch.name {
            body.insert("name".into(), json!(n));
        }
        if let Some(p) = &patch.parent_id {
            body.insert("parentReference".into(), json!({ "id": p }));
        }
        if patch.modified.is_some() || patch.created.is_some() {
            let fsi = FileSystemInfo { created_date_time: patch.created, last_modified_date_time: patch.modified };
            body.insert("fileSystemInfo".into(), serde_json::to_value(fsi).unwrap_or_default());
        }
        let body = serde_json::Value::Object(body);
        let resp = self
            .execute(|c| {
                let mut rb = c.patch(&url).json(&body);
                if let Some(tag) = if_match {
                    rb = rb.header(header::IF_MATCH, tag);
                }
                rb
            })
            .await?;
        parse_json(resp).await
    }

    /// Delete (moves to the OneDrive recycle bin). A 404 counts as success.
    pub async fn delete_item(&self, item_id: &str, if_match: Option<&str>) -> Result<()> {
        let url = self.drive_url(&format!("/items/{item_id}"));
        match self
            .execute(|c| {
                let mut rb = c.delete(&url);
                if let Some(tag) = if_match {
                    rb = rb.header(header::IF_MATCH, tag);
                }
                rb
            })
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.is_not_found() => Ok(()),
            Err(e) => Err(e),
        }
    }

    // ---------------------------------------------------------------- extras

    /// Create (or return the existing) sharing link.
    pub async fn create_link(&self, item_id: &str, kind: &str, scope: &str) -> Result<String> {
        let url = self.drive_url(&format!("/items/{item_id}/createLink"));
        let body = json!({ "type": kind, "scope": scope });
        let resp = self.execute(|c| c.post(&url).json(&body)).await?;
        let p: crate::sharing::Permission = parse_json(resp).await?;
        p.link.and_then(|l| l.web_url).ok_or_else(|| GraphError::InvalidResponse("createLink returned no URL".into()))
    }

    pub async fn versions(&self, item_id: &str) -> Result<Vec<DriveItemVersion>> {
        let list: ValueList<DriveItemVersion> =
            self.get_json(&self.drive_url(&format!("/items/{item_id}/versions"))).await?;
        Ok(list.value)
    }

    pub async fn restore_version(&self, item_id: &str, version_id: &str) -> Result<()> {
        let url = self.drive_url(&format!("/items/{item_id}/versions/{version_id}/restoreVersion"));
        self.execute(|c| c.post(&url).header(header::CONTENT_LENGTH, 0)).await?;
        Ok(())
    }

    pub async fn thumbnails(&self, item_id: &str) -> Result<Vec<ThumbnailSet>> {
        let list: ValueList<ThumbnailSet> =
            self.get_json(&self.drive_url(&format!("/items/{item_id}/thumbnails"))).await?;
        Ok(list.value)
    }

    /// Fetch bytes from a pre-authenticated URL (thumbnails).
    pub async fn fetch_unauthenticated(&self, url: &str) -> Result<Bytes> {
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp).await);
        }
        Ok(resp.bytes().await?)
    }

    /// Endpoint for real-time change notifications. They are offered for a
    /// drive's root only, so a shared folder listens to its whole drive.
    pub async fn socket_io_endpoint(&self) -> Result<SocketIoEndpoint> {
        self.get_json(&self.drive_url("/root/subscriptions/socketIo")).await
    }
}

/// Exponential backoff with jitter: 1, 2, 4, 8, … seconds (capped at 5 minutes).
pub fn backoff(attempt: u32) -> Duration {
    use rand::Rng;
    let base = 1u64 << attempt.saturating_sub(1).min(8);
    let jitter = rand::rng().random_range(0..=500);
    Duration::from_millis((base * 1000 + jitter).min(300_000))
}

/// Percent-encode a single path segment for Graph path addressing.
pub fn encode_segment(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub(crate) async fn parse_json<T: DeserializeOwned>(resp: Response) -> Result<T> {
    let bytes = resp.bytes().await?;
    serde_json::from_slice(&bytes).map_err(|e| {
        GraphError::InvalidResponse(format!("{e}: {}", String::from_utf8_lossy(&bytes[..bytes.len().min(300)])))
    })
}

pub(crate) async fn error_from_response(resp: Response) -> GraphError {
    let status = resp.status().as_u16();
    let location = resp.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()).map(str::to_string);
    let retry_after = resp
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let text = resp.text().await.unwrap_or_default();
    let env: ErrorEnvelope = serde_json::from_str(&text).unwrap_or_default();
    let (code, inner_code, message) = match env.error {
        Some(b) => {
            let inner = b.innermost_code().to_string();
            (b.code.clone(), inner, b.message.clone())
        }
        None => (String::new(), String::new(), text.chars().take(200).collect()),
    };
    let resync = |c: &str| c.to_ascii_lowercase().contains("resync");
    if status == 410 || resync(&code) || resync(&inner_code) {
        return GraphError::ResyncRequired {
            code: if inner_code.is_empty() { code } else { inner_code },
            restart_link: if status == 410 { location } else { None },
        };
    }
    GraphError::Api { status, code, inner_code, message, retry_after }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_encoded() {
        assert_eq!(encode_segment("a b#c%.txt"), "a%20b%23c%25.txt");
        assert_eq!(encode_segment("ü"), "%C3%BC");
    }

    #[test]
    fn drive_ids_are_normalised() {
        assert_eq!(normalize_drive_id("ABCDEF0123456789"), "abcdef0123456789");
        assert_eq!(normalize_drive_id("BCDEF0123456789"), "0bcdef0123456789");
        assert_eq!(normalize_drive_id("b!xyz-Long_Business_Id"), "b!xyz-Long_Business_Id");
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert!(backoff(1) >= Duration::from_secs(1));
        assert!(backoff(4) >= Duration::from_secs(8));
        assert!(backoff(30) <= Duration::from_secs(300));
    }
}
