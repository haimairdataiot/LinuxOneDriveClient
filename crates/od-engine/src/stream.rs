//! Streaming hydration for large online-only files.
//!
//! Instead of making an application wait for a multi-gigabyte download, the
//! file is fetched in fixed-size blocks into a sparse partial file:
//! * a read waits only for the blocks it needs (fetched first, out of order,
//!   so seeking in a video works);
//! * a background filler downloads the remaining blocks in order;
//! * when every block is present, the whole file is verified against the
//!   server's QuickXorHash and installed atomically, exactly like a normal
//!   download. A mismatch discards everything; nothing unverified becomes
//!   the file's cached content.

use crate::engine::{Engine, EngineError, EngineResult, Inval};
use crate::store;
use bytes::Bytes;
use od_core::types::ActivityKind;
use std::collections::HashSet;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Block size for streamed downloads.
pub const BLOCK: u64 = 4 * 1024 * 1024;
/// Files at least this large are streamed rather than downloaded first.
pub const STREAM_MIN: u64 = 16 * 1024 * 1024;
/// Parallel block fetches for one file (on-demand + background).
const MAX_INFLIGHT: usize = 3;

#[derive(Default)]
struct State {
    have: Vec<bool>,
    inflight: HashSet<usize>,
    /// Set when the stream failed permanently (errno, message).
    failed: Option<(i32, String)>,
    finished: bool,
    received: u64,
}

pub struct Stream {
    ino: u64,
    name: String,
    item_id: String,
    size: u64,
    expected_hash: Option<String>,
    remote_ctag: Option<String>,
    path: PathBuf,
    file: Arc<File>,
    state: Mutex<State>,
    changed: Notify,
    url: tokio::sync::Mutex<Option<(String, Instant)>>,
}

impl Stream {
    fn blocks(&self) -> usize {
        self.size.div_ceil(BLOCK) as usize
    }

    fn range(&self, b: usize) -> (u64, u64) {
        let start = b as u64 * BLOCK;
        (start, (start + BLOCK).min(self.size) - 1)
    }

    pub fn is_finished(&self) -> bool {
        self.state.lock().unwrap().finished
    }

    fn failure(&self) -> Option<(i32, String)> {
        self.state.lock().unwrap().failed.clone()
    }
}

impl Engine {
    /// Start (or join) streaming `ino`; `None` if it isn't a candidate.
    pub(crate) fn stream_for(self: &Arc<Self>, ino: u64) -> EngineResult<Option<Arc<Stream>>> {
        if let Some(s) = self.streams.lock().unwrap().get(&ino) {
            return Ok(Some(s.clone()));
        }
        let row = store::get_live(&self.store.lock(), ino)?.ok_or(store::StoreError::NotFound)?;
        let size = row.remote_size.unwrap_or(row.size);
        let Some(item_id) = row.item_id.clone().or_else(|| row.origin_id.clone()) else { return Ok(None) };
        if row.is_dir || row.hydrated || row.dirty || size < STREAM_MIN {
            return Ok(None);
        }
        let free = self.content.free_space().unwrap_or(u64::MAX);
        if free < size.saturating_add(self.cfg().disk_reserve) {
            return Err(EngineError::NoSpace);
        }
        let path = self.content.partial_path(ino);
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path)?;
        file.set_len(size)?; // sparse
        let stream = Arc::new(Stream {
            ino,
            name: row.name.clone(),
            item_id,
            size,
            expected_hash: row.remote_hash.clone(),
            remote_ctag: row.ctag.clone(),
            path,
            file: Arc::new(file),
            state: Mutex::new(State::default()),
            changed: Notify::new(),
            url: tokio::sync::Mutex::new(None),
        });
        stream.state.lock().unwrap().have = vec![false; stream.blocks()];
        let mut map = self.streams.lock().unwrap();
        if let Some(existing) = map.get(&ino) {
            let _ = std::fs::remove_file(&stream.path);
            return Ok(Some(existing.clone()));
        }
        map.insert(ino, stream.clone());
        drop(map);
        self.transfer_begin(ino, &row.name, size, false);
        tokio::spawn(self.clone().stream_filler(stream.clone()));
        Ok(Some(stream))
    }

    /// Read `[offset, offset+len)` of a streamed file, waiting only for the
    /// blocks that cover it.
    pub(crate) async fn stream_read(
        self: &Arc<Self>,
        stream: &Arc<Stream>,
        offset: u64,
        len: u32,
    ) -> EngineResult<Vec<u8>> {
        if offset >= stream.size {
            return Ok(Vec::new());
        }
        let end = (offset + len as u64).min(stream.size);
        let first = (offset / BLOCK) as usize;
        let last = ((end - 1) / BLOCK) as usize;
        loop {
            let notified = stream.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some((errno, msg)) = stream.failure() {
                return Err(EngineError::Shared { errno, msg });
            }
            let missing: Vec<usize> = {
                let st = stream.state.lock().unwrap();
                (first..=last).filter(|b| !st.have[*b]).collect()
            };
            if missing.is_empty() {
                break;
            }
            for b in missing {
                self.clone().fetch_block_spawn(stream.clone(), b);
            }
            notified.await;
        }
        let mut buf = vec![0u8; (end - offset) as usize];
        let f = stream.file.clone();
        let buf = tokio::task::spawn_blocking(move || f.read_exact_at(&mut buf, offset).map(|_| buf))
            .await
            .map_err(|e| EngineError::Other(e.to_string()))??;
        Ok(buf)
    }

    /// Wait until a stream has been verified and installed (or failed).
    pub(crate) async fn stream_wait(&self, stream: &Arc<Stream>) -> EngineResult<()> {
        loop {
            let notified = stream.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some((errno, msg)) = stream.failure() {
                return Err(EngineError::Shared { errno, msg });
            }
            if stream.is_finished() {
                return Ok(());
            }
            notified.await;
        }
    }

    fn fetch_block_spawn(self: Arc<Self>, stream: Arc<Stream>, b: usize) {
        {
            let mut st = stream.state.lock().unwrap();
            if st.have[b] || st.inflight.contains(&b) || st.failed.is_some() {
                return;
            }
            st.inflight.insert(b);
        }
        tokio::spawn(async move {
            let r = self.fetch_block(&stream, b).await;
            let mut st = stream.state.lock().unwrap();
            st.inflight.remove(&b);
            match r {
                Ok(n) => {
                    st.have[b] = true;
                    st.received += n;
                    let received = st.received;
                    drop(st);
                    self.transfer_progress(stream.ino, received);
                }
                Err(e) => {
                    tracing::warn!(ino = stream.ino, block = b, "streamed block failed: {e}");
                    st.failed = Some((e.errno(), e.to_string()));
                }
            }
            stream.changed.notify_waiters();
        });
    }

    async fn url(&self, stream: &Stream, refresh: bool) -> EngineResult<String> {
        let mut u = stream.url.lock().await;
        if let Some((url, at)) = u.as_ref() {
            if !refresh && at.elapsed() < Duration::from_secs(300) {
                return Ok(url.clone());
            }
        }
        let url = self.graph.download_url(&stream.item_id).await?;
        *u = Some((url.clone(), Instant::now()));
        Ok(url)
    }

    async fn fetch_block(&self, stream: &Stream, b: usize) -> EngineResult<u64> {
        let (start, end) = stream.range(b);
        let mut last = None;
        for attempt in 0..4u32 {
            let url = self.url(stream, attempt > 0).await?;
            match self.graph.fetch_range(&url, start, end).await {
                Ok(bytes) => {
                    let f = stream.file.clone();
                    let n = bytes.len() as u64;
                    tokio::task::spawn_blocking(move || write_block(&f, start, &bytes))
                        .await
                        .map_err(|e| EngineError::Other(e.to_string()))??;
                    return Ok(n);
                }
                Err(e) if e.is_offline() => {
                    self.set_offline(true);
                    return Err(e.into());
                }
                Err(e) => {
                    last = Some(e);
                    tokio::time::sleep(Duration::from_millis(300 * (attempt as u64 + 1))).await;
                }
            }
        }
        Err(last.map(EngineError::from).unwrap_or_else(|| EngineError::Other("block download failed".into())))
    }

    /// Download remaining blocks in order, then verify and install.
    async fn stream_filler(self: Arc<Self>, stream: Arc<Stream>) {
        loop {
            let notified = stream.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let (next, inflight, complete, failed) = {
                let st = stream.state.lock().unwrap();
                let next = (0..st.have.len()).find(|b| !st.have[*b] && !st.inflight.contains(b));
                (next, st.inflight.len(), st.have.iter().all(|h| *h), st.failed.is_some())
            };
            if failed {
                break;
            }
            if complete {
                let r = self.finish_stream(&stream).await;
                let mut st = stream.state.lock().unwrap();
                match r {
                    Ok(()) => st.finished = true,
                    Err(e) => st.failed = Some((e.errno(), e.to_string())),
                }
                break;
            }
            if self.cancel.is_cancelled() {
                break;
            }
            match next {
                Some(b) if inflight < MAX_INFLIGHT => self.clone().fetch_block_spawn(stream.clone(), b),
                _ => notified.await,
            }
        }
        if stream.failure().is_some() {
            let _ = std::fs::remove_file(&stream.path);
        }
        self.streams.lock().unwrap().remove(&stream.ino);
        self.transfer_end(stream.ino);
        stream.changed.notify_waiters();
    }

    /// Verify the assembled file and make it the cached content.
    async fn finish_stream(&self, stream: &Arc<Stream>) -> EngineResult<()> {
        let f = stream.file.clone();
        let size = stream.size;
        let hash = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
            f.sync_all()?;
            let mut h = od_core::quickxor::QuickXorHash::new();
            let mut buf = vec![0u8; 1 << 20];
            let mut off = 0;
            while off < size {
                let n = f.read_at(&mut buf[..(1 << 20).min((size - off) as usize)], off)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
                off += n as u64;
            }
            Ok(h.finalize_base64())
        })
        .await
        .map_err(|e| EngineError::Other(e.to_string()))??;
        if let Some(expected) = &stream.expected_hash {
            if *expected != hash {
                return Err(EngineError::Graph(od_graph::GraphError::HashMismatch));
            }
        }
        self.content.tag_path(&stream.path, &stream.name, false);
        let installed = {
            let c = self.store.lock();
            match store::get_live(&c, stream.ino)? {
                Some(n)
                    if n.remote_hash == stream.expected_hash
                        && n.ctag == stream.remote_ctag
                        && !n.dirty
                        && !self.is_busy(stream.ino) =>
                {
                    self.content.install(stream.ino, &stream.path)?;
                    store::set_hydrated(&c, stream.ino, true, Some(&hash))?;
                    if n.item_id.is_none() && n.origin_id.is_some() {
                        c.execute("UPDATE items SET origin_id = NULL WHERE ino = ?1", [stream.ino as i64])?;
                    }
                    true
                }
                _ => false,
            }
        };
        if !installed {
            return Err(EngineError::Other("the file changed while it was downloading".into()));
        }
        self.clear_thumbnail_failure(stream.ino);
        self.invalidate(vec![Inval::Inode { ino: stream.ino }]);
        self.items_changed(&[stream.ino]);
        let rel = store::rel_path(&self.store.lock(), stream.ino).unwrap_or_default();
        self.activity(ActivityKind::Downloaded, Some(stream.ino), &rel, size, "");
        Ok(())
    }
}

fn write_block(f: &File, offset: u64, data: &Bytes) -> std::io::Result<()> {
    f.write_all_at(data, offset)
}
