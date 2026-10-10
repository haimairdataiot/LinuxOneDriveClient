//! Test harness: a real FUSE mount of the engine against the mock OneDrive.

#![allow(dead_code)]

use od_core::types::DriveType;
use od_engine::{Engine, EngineConfig};
use od_graph::client::build_http_client;
use od_graph::{GraphClient, TokenManager};
use od_mockgraph::{MockGraph, VALID_TOKEN};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub struct Harness {
    pub mock: MockGraph,
    pub engine: Arc<Engine>,
    pub mnt: PathBuf,
    pub account_dir: PathBuf,
    session: Option<fuser::BackgroundSession>,
    _dir: tempfile::TempDir,
}

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .with_test_writer()
        .try_init();
}

impl Harness {
    pub async fn start() -> Harness {
        Self::start_with(MockGraph::start().await, None, |_| {}).await
    }

    pub async fn start_with(
        mock: MockGraph,
        dir: Option<tempfile::TempDir>,
        tweak: impl FnOnce(&mut EngineConfig),
    ) -> Harness {
        init_tracing();
        let dir = dir.unwrap_or_else(|| {
            tempfile::Builder::new().prefix("lod-").tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
        });
        let account_dir = dir.path().join("account");
        let mnt = dir.path().join("OneDrive");
        std::fs::create_dir_all(&mnt).unwrap();
        let http = build_http_client();
        let tokens = Arc::new(TokenManager::fixed(http.clone(), VALID_TOKEN));
        let graph = GraphClient::new(http, mock.base_url(), tokens).with_max_attempts(3);
        let mut cfg = EngineConfig::new("test", &account_dir, &mnt, od_mockgraph::DRIVE_ID, DriveType::Personal);
        cfg.poll_interval = Duration::from_millis(500);
        cfg.upload_quiet = Duration::from_millis(150);
        cfg.hostname = "testhost".into();
        tweak(&mut cfg);
        let engine = Engine::new(cfg, graph).expect("engine");
        engine.pull_once().await.expect("initial pull");
        let session = od_engine::vfs::mount(engine.clone(), tokio::runtime::Handle::current()).expect("mount");
        engine.start();
        Harness { mock, engine, mnt, account_dir, session: Some(session), _dir: dir }
    }

    /// A second engine syncing folder `root` of drive `drive_id` (a shortcut
    /// to a shared folder), mounted in its own directory.
    pub async fn start_scoped(
        mock: MockGraph,
        drive_id: &str,
        root: &str,
        drive_type: DriveType,
        walk: bool,
    ) -> Harness {
        Self::start_scoped_in(mock, None, drive_id, root, drive_type, walk).await
    }

    /// Like [`Self::start_scoped`], optionally on the state of an earlier run.
    pub async fn start_scoped_in(
        mock: MockGraph,
        dir: Option<tempfile::TempDir>,
        drive_id: &str,
        root: &str,
        drive_type: DriveType,
        walk: bool,
    ) -> Harness {
        init_tracing();
        let dir = dir.unwrap_or_else(|| {
            tempfile::Builder::new().prefix("lod-sc-").tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
        });
        let account_dir = dir.path().join("account");
        let mnt = dir.path().join("Shared");
        std::fs::create_dir_all(&mnt).unwrap();
        let http = build_http_client();
        let tokens = Arc::new(TokenManager::fixed(http.clone(), VALID_TOKEN));
        let graph =
            GraphClient::new(http, mock.base_url(), tokens).with_max_attempts(3).with_drive(drive_id).with_root(root);
        let mut cfg = EngineConfig::new("test-sc", &account_dir, &mnt, drive_id, drive_type);
        cfg.poll_interval = Duration::from_millis(500);
        cfg.upload_quiet = Duration::from_millis(150);
        cfg.hostname = "testhost".into();
        cfg.walk_listing = walk;
        cfg.listing_interval = Duration::ZERO;
        cfg.track_quota = false;
        cfg.push_notifications = false;
        let engine = Engine::new(cfg, graph).expect("engine");
        engine.pull_once().await.expect("initial pull");
        let session = od_engine::vfs::mount(engine.clone(), tokio::runtime::Handle::current()).expect("mount");
        engine.start();
        Harness { mock, engine, mnt, account_dir, session: Some(session), _dir: dir }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.mnt.join(rel.trim_start_matches('/'))
    }

    /// Unmount and stop, keeping the temp directory (for restart tests).
    pub async fn stop(mut self) -> (MockGraph, tempfile::TempDir) {
        self.engine.stop();
        if let Some(s) = self.session.take() {
            tokio::task::spawn_blocking(move || {
                let _ = s.umount_and_join();
            })
            .await
            .unwrap();
        }
        let dir = std::mem::replace(&mut self._dir, tempfile::tempdir().unwrap());
        (self.mock.clone(), dir)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.engine.stop();
        if self.session.is_some() {
            let _ = std::process::Command::new("fusermount3").arg("-uz").arg(&self.mnt).status();
        }
    }
}

/// Run blocking filesystem code off the async runtime (the FUSE server needs it).
pub async fn fs<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

/// Poll `cond` (run on a blocking thread) until it holds or the timeout expires.
pub async fn wait_for(what: &str, timeout_secs: u64, cond: impl Fn() -> bool + Send + Sync + 'static) {
    let cond = Arc::new(cond);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let c = cond.clone();
        if tokio::task::spawn_blocking(move || c()).await.unwrap() {
            return;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub fn getxattr(p: &Path, name: &str) -> Option<String> {
    od_engine::content::get_xattr(p, name).ok().map(|v| String::from_utf8_lossy(&v).into_owned())
}

pub fn setxattr(p: &Path, name: &str, value: &str) -> std::io::Result<()> {
    od_engine::content::set_xattr(p, name, value.as_bytes())
}
