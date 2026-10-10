//! Fault-injection tests (research doc 04 §12 matrix): lost replies, crashes
//! mid-upload, corrupt or truncated downloads, full disks, conflicting
//! deletes, resyncs, throttling and a damaged database.

mod common;

use common::*;
use od_mockgraph::{FaultKind, MockGraph};
use std::time::Duration;

const STATUS: &str = "user.onedrive.status";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_upload_reply_does_not_duplicate() {
    let h = Harness::start().await;
    // The server commits the upload but the reply never arrives.
    h.mock.fault("/_upload/", FaultKind::AfterCommit, 1);
    let p = h.path("report.txt");
    fs(move || std::fs::write(&p, b"exactly once")).await.unwrap();
    let p = h.path("report.txt");
    wait_for("clean after lost reply", 15, move || getxattr(&p, STATUS).as_deref() == Some("local")).await;
    assert_eq!(h.mock.list("/"), vec!["report.txt"], "no duplicate like 'report 1.txt'");
    assert_eq!(h.mock.read("/report.txt").unwrap(), b"exactly once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupted_upload_resumes_after_restart() {
    let h = Harness::start().await;
    // Fragment 1 succeeds, fragment 2 fails hard: the session must survive a restart.
    h.mock.fault_after("/_upload/", FaultKind::Error, 400, 1);
    let size = od_graph::client::UPLOAD_FRAGMENT * 5 / 2;
    let data: Vec<u8> = (0..size).map(|i| (i % 253) as u8).collect();
    let p = h.path("big.bin");
    let d2 = data.clone();
    fs(move || std::fs::write(&p, &d2)).await.unwrap();
    let m = h.mock.clone();
    wait_for("first fragment received", 15, move || {
        m.stat(|s| s.bytes_uploaded) >= od_graph::client::UPLOAD_FRAGMENT as u64
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (mock, dir) = h.stop().await;
    assert!(!mock.exists("/big.bin"));

    let h = Harness::start_with(mock, Some(dir), |_| {}).await;
    h.engine.retry_failed().unwrap();
    let m = h.mock.clone();
    let expected = data.clone();
    wait_for("upload resumed and completed", 30, move || m.read("/big.bin").as_deref() == Some(&expected[..])).await;
    assert_eq!(h.mock.stat(|s| s.bytes_uploaded), size as u64, "no byte was sent twice");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_download_is_never_installed() {
    let mock = MockGraph::start().await;
    mock.put_file("/photo.jpg", &vec![9u8; 50_000]);
    let h = Harness::start_with(mock, None, |_| {}).await;
    h.mock.fault("/_download/", FaultKind::CorruptBody, 50);
    let p = h.path("photo.jpg");
    let err = fs(move || std::fs::read(&p)).await.unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EIO), "{err}");
    h.mock.clear_faults();
    let p = h.path("photo.jpg");
    assert_eq!(
        fs(move || getxattr(&p, STATUS)).await.as_deref(),
        Some("online"),
        "corrupt bytes never become the file"
    );
    // Once the server behaves, the file opens normally.
    let p = h.path("photo.jpg");
    assert_eq!(fs(move || std::fs::read(&p).unwrap()).await, vec![9u8; 50_000]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn truncated_download_is_retried() {
    let mock = MockGraph::start().await;
    mock.put_file("/doc.pdf", &vec![3u8; 80_000]);
    let h = Harness::start_with(mock, None, |_| {}).await;
    h.mock.fault("/_download/", FaultKind::TruncateBody, 1);
    let p = h.path("doc.pdf");
    assert_eq!(fs(move || std::fs::read(&p).unwrap()).await, vec![3u8; 80_000]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_disk_fails_cleanly() {
    let mock = MockGraph::start().await;
    mock.put_file("/movie.mkv", &vec![1u8; 10_000]);
    let h = Harness::start_with(mock, None, |c| {
        c.disk_reserve = u64::MAX / 4;
        c.critical_free = u64::MAX / 4;
    })
    .await;
    let p = h.path("movie.mkv");
    let err = fs(move || std::fs::read(&p)).await.unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::ENOSPC));
    let p = h.path("new.txt");
    let err = fs(move || std::fs::write(&p, b"no room")).await.unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::ENOSPC));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_delete_of_modified_file_keeps_local_changes() {
    let mock = MockGraph::start().await;
    mock.put_file("/plan.txt", b"v1");
    let h = Harness::start_with(mock, None, |_| {}).await;
    h.engine.set_paused(od_engine::engine::pause::USER, true);
    let p = h.path("plan.txt");
    fs(move || std::fs::write(&p, b"my local edits")).await.unwrap();
    h.mock.remove("/plan.txt");
    h.engine.set_paused(od_engine::engine::pause::USER, false);
    h.engine.refresh();
    let m = h.mock.clone();
    wait_for("re-uploaded as new", 15, move || m.read("/plan.txt").as_deref() == Some(&b"my local edits"[..])).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resync_keeps_unsynced_files_and_learns_remote_ones() {
    let mock = MockGraph::start().await;
    mock.put_file("/a.txt", b"a");
    let h = Harness::start_with(mock, None, |_| {}).await;
    h.engine.set_paused(od_engine::engine::pause::USER, true);
    let p = h.path("local-only-so-far.txt");
    fs(move || std::fs::write(&p, b"precious")).await.unwrap();
    h.mock.expire_delta_tokens();
    h.mock.put_file("/b.txt", b"b");
    h.engine.set_paused(od_engine::engine::pause::USER, false);
    h.engine.refresh();
    let (m, pb) = (h.mock.clone(), h.path("b.txt"));
    wait_for("resync done", 15, move || pb.exists() && m.read("/local-only-so-far.txt").is_some()).await;
    assert!(h.path("a.txt").exists());
    assert_eq!(h.mock.read("/local-only-so-far.txt").unwrap(), b"precious");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn throttling_is_not_an_item_error() {
    let h = Harness::start().await;
    h.mock.fail("/me/drive/items", 429, 3);
    let p = h.path("x.txt");
    fs(move || std::fs::write(&p, b"x")).await.unwrap();
    let m = h.mock.clone();
    wait_for("uploaded despite throttling", 20, move || m.exists("/x.txt")).await;
    let issues = od_engine::store::issue_count(&h.engine.store.lock()).unwrap();
    assert_eq!(issues, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn damaged_database_is_recovered_and_unsynced_files_salvaged() {
    let h = Harness::start().await;
    h.engine.set_paused(od_engine::engine::pause::USER, true);
    let p = h.path("unsynced.txt");
    fs(move || std::fs::write(&p, b"do not lose me")).await.unwrap();
    let p = h.path("second.txt");
    fs(move || std::fs::write(&p, b"me neither")).await.unwrap();
    let account_dir = h.account_dir.clone();
    let (mock, dir) = h.stop().await;

    // Destroy the database (no backups exist yet).
    std::fs::write(account_dir.join("state.db"), vec![0x42u8; 16384]).unwrap();
    let _ = std::fs::remove_file(account_dir.join("state.db-wal"));
    let _ = std::fs::remove_file(account_dir.join("state.db-shm"));

    let h = Harness::start_with(mock, Some(dir), |_| {}).await;
    let folder = format!("/Recovered files ({})", chrono::Local::now().format("%Y-%m-%d"));
    let path = format!("{folder}/unsynced.txt");
    let m = h.mock.clone();
    let p2 = path.clone();
    let second = format!("{folder}/second.txt");
    wait_for("salvaged files uploaded", 20, move || {
        m.read(&p2).as_deref() == Some(&b"do not lose me"[..]) && m.read(&second).as_deref() == Some(&b"me neither"[..])
    })
    .await;
    let kept: Vec<_> = std::fs::read_dir(&account_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("state.db.corrupt-"))
        .collect();
    assert!(!kept.is_empty(), "the damaged database is kept for diagnosis, never deleted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn integrity_marker_triggers_recovery_on_restart() {
    let h = Harness::start().await;
    let p = h.path("keep.txt");
    fs(move || std::fs::write(&p, b"k")).await.unwrap();
    let m = h.mock.clone();
    wait_for("uploaded", 10, move || m.exists("/keep.txt")).await;
    let account_dir = h.account_dir.clone();
    h.engine.backup_db_now();
    let (mock, dir) = h.stop().await;
    std::fs::write(account_dir.join("state.db.needs-repair"), b"test").unwrap();
    let h = Harness::start_with(mock, Some(dir), |_| {}).await;
    assert!(!account_dir.join("state.db.needs-repair").exists());
    let p = h.path("keep.txt");
    assert_eq!(fs(move || std::fs::read(&p).unwrap()).await, b"k", "restored from the backup");
}
