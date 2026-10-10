//! End-to-end: real FUSE mount + engine + mock OneDrive.

mod common;

use common::*;
use std::os::unix::fs::MetadataExt;

const STATUS: &str = "user.onedrive.status";
const PIN: &str = "user.onedrive.pin";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browse_hydrate_and_status() {
    let mock = od_mockgraph::MockGraph::start().await;
    mock.put_file("/Docs/hello.txt", b"hello world");
    mock.put_file("/Docs/Sub/deep.bin", &vec![7u8; 100_000]);
    let h = Harness::start_with(mock, None, |_| {}).await;

    let docs = h.path("Docs");
    let names = fs(move || {
        let mut v: Vec<String> =
            std::fs::read_dir(&docs).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        v.sort();
        v
    })
    .await;
    assert_eq!(names, vec!["Sub", "hello.txt"]);

    // Online-only: real size, no blocks on disk, nothing downloaded yet.
    let hello = h.path("Docs/hello.txt");
    let (size, blocks, status) = {
        let p = hello.clone();
        fs(move || {
            let m = std::fs::metadata(&p).unwrap();
            (m.len(), m.blocks(), getxattr(&p, STATUS))
        })
        .await
    };
    assert_eq!((size, blocks), (11, 0));
    assert_eq!(status.as_deref(), Some("online"));
    assert_eq!(h.mock.stat(|s| s.downloads), 0);

    // Reading hydrates (once) and the status becomes "local".
    let p = hello.clone();
    assert_eq!(fs(move || std::fs::read_to_string(&p).unwrap()).await, "hello world");
    let p = hello.clone();
    assert_eq!(fs(move || std::fs::read_to_string(&p).unwrap()).await, "hello world");
    assert_eq!(h.mock.stat(|s| s.downloads), 1);
    let p = hello.clone();
    assert_eq!(fs(move || getxattr(&p, STATUS)).await.as_deref(), Some("local"));

    // .trackerignore keeps desktop search out; it is never uploaded.
    let ti = h.path(".trackerignore");
    assert!(fs(move || ti.exists()).await);
    assert!(!h.mock.exists("/.trackerignore"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_modify_atomic_save_rename_delete() {
    let mock = od_mockgraph::MockGraph::start().await;
    mock.put_file("/Docs/report.txt", b"v1");
    let h = Harness::start_with(mock, None, |_| {}).await;
    let original_id = h.mock.id_of("/Docs/report.txt").unwrap();

    // New file is uploaded.
    let p = h.path("Docs/new.txt");
    fs(move || std::fs::write(&p, b"brand new")).await.unwrap();
    let m = h.mock.clone();
    wait_for("new.txt uploaded", 10, move || m.read("/Docs/new.txt").as_deref() == Some(&b"brand new"[..])).await;

    // In-place modification keeps the item id (version history).
    let p = h.path("Docs/report.txt");
    fs(move || std::fs::write(&p, b"v2 edited in place")).await.unwrap();
    let m = h.mock.clone();
    wait_for("report v2", 10, move || m.read("/Docs/report.txt").as_deref() == Some(&b"v2 edited in place"[..])).await;
    assert_eq!(h.mock.id_of("/Docs/report.txt").unwrap(), original_id);

    // Editor-style atomic save: write a temp file, rename it over the original.
    let dir = h.path("Docs");
    fs(move || {
        std::fs::write(dir.join(".report.txt.swp~tmp"), b"v3 atomic").unwrap();
        std::fs::rename(dir.join(".report.txt.swp~tmp"), dir.join("report.txt")).unwrap();
    })
    .await;
    let m = h.mock.clone();
    wait_for("report v3", 10, move || m.read("/Docs/report.txt").as_deref() == Some(&b"v3 atomic"[..])).await;
    assert_eq!(h.mock.id_of("/Docs/report.txt").unwrap(), original_id, "atomic save must keep the item id");

    // Folder creation, move and rename.
    let root = h.mnt.clone();
    fs(move || {
        std::fs::create_dir(root.join("Archive")).unwrap();
        std::fs::rename(root.join("Docs/new.txt"), root.join("Archive/renamed.txt")).unwrap();
    })
    .await;
    let m = h.mock.clone();
    wait_for("moved", 10, move || m.exists("/Archive/renamed.txt") && !m.exists("/Docs/new.txt")).await;

    // Delete goes to the server (recycle bin).
    let p = h.path("Archive/renamed.txt");
    fs(move || std::fs::remove_file(&p)).await.unwrap();
    let m = h.mock.clone();
    wait_for("deleted online", 10, move || !m.exists("/Archive/renamed.txt")).await;
    let p = h.path("Archive");
    fs(move || std::fs::remove_dir(&p)).await.unwrap();
    let m = h.mock.clone();
    wait_for("folder deleted online", 10, move || !m.exists("/Archive")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_changes_appear_locally() {
    let h = Harness::start().await;
    h.mock.put_file("/Photos/a.jpg", b"jpeg-1");
    h.engine.refresh();
    let p = h.path("Photos/a.jpg");
    wait_for("remote file appears", 10, move || p.exists()).await;

    // Read it (hydrated), then change it online: the new content shows up.
    let p = h.path("Photos/a.jpg");
    assert_eq!(fs(move || std::fs::read(&p).unwrap()).await, b"jpeg-1");
    h.mock.put_file("/Photos/a.jpg", b"jpeg-2-longer");
    h.engine.refresh();
    let p = h.path("Photos/a.jpg");
    wait_for("remote edit visible", 10, move || std::fs::read(&p).map(|b| b == b"jpeg-2-longer").unwrap_or(false))
        .await;

    // Remote rename and delete.
    h.mock.rename("/Photos/a.jpg", "/Photos/b.jpg");
    h.engine.refresh();
    let (pa, pb) = (h.path("Photos/a.jpg"), h.path("Photos/b.jpg"));
    wait_for("remote rename", 10, move || !pa.exists() && pb.exists()).await;
    h.mock.remove("/Photos");
    h.engine.refresh();
    let p = h.path("Photos");
    wait_for("remote delete", 10, move || !p.exists()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conflicting_edits_keep_both() {
    let mock = od_mockgraph::MockGraph::start().await;
    mock.put_file("/notes.txt", b"base");
    let h = Harness::start_with(mock, None, |_| {}).await;
    let p = h.path("notes.txt");
    assert_eq!(
        fs({
            let p = p.clone();
            move || std::fs::read(&p).unwrap()
        })
        .await,
        b"base"
    );

    // Pause, edit on both sides, resume.
    h.engine.set_paused(od_engine::engine::pause::USER, true);
    fs({
        let p = p.clone();
        move || std::fs::write(&p, b"local edit")
    })
    .await
    .unwrap();
    h.mock.put_file("/notes.txt", b"remote edit");
    h.engine.set_paused(od_engine::engine::pause::USER, false);
    h.engine.refresh();

    let m = h.mock.clone();
    wait_for("conflict copy uploaded", 15, move || {
        m.read("/notes-testhost.txt").as_deref() == Some(&b"local edit"[..])
    })
    .await;
    assert_eq!(h.mock.read("/notes.txt").unwrap(), b"remote edit", "the server version keeps the name");
    let p2 = h.path("notes.txt");
    wait_for("local shows server version", 10, move || {
        std::fs::read(&p2).map(|b| b == b"remote edit").unwrap_or(false)
    })
    .await;
    let c = h.path("notes-testhost.txt");
    assert_eq!(fs(move || std::fs::read(&c).unwrap()).await, b"local edit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pin_and_free_up_space() {
    let mock = od_mockgraph::MockGraph::start().await;
    for i in 0..3 {
        mock.put_file(&format!("/Keep/f{i}.dat"), format!("data{i}").as_bytes());
    }
    let h = Harness::start_with(mock, None, |_| {}).await;
    let keep = h.path("Keep");
    fs({
        let k = keep.clone();
        move || setxattr(&k, PIN, "pinned")
    })
    .await
    .unwrap();
    let f0 = h.path("Keep/f0.dat");
    wait_for("pinned files downloaded", 10, move || getxattr(&f0, STATUS).as_deref() == Some("pinned")).await;
    assert_eq!(h.mock.stat(|s| s.downloads), 3);
    let f2 = h.path("Keep/f2.dat");
    assert!(fs(move || std::fs::metadata(&f2).unwrap().blocks() > 0).await);

    // Free up space: content dropped, file still listed and readable on demand.
    fs({
        let k = keep.clone();
        move || setxattr(&k, PIN, "unpinned")
    })
    .await
    .unwrap();
    let f1 = h.path("Keep/f1.dat");
    let (status, blocks) = fs(move || (getxattr(&f1, STATUS), std::fs::metadata(&f1).unwrap().blocks())).await;
    assert_eq!(status.as_deref(), Some("online"));
    assert_eq!(blocks, 0);
    let f1 = h.path("Keep/f1.dat");
    assert_eq!(fs(move || std::fs::read(&f1).unwrap()).await, b"data1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mass_delete_is_held_and_restorable() {
    let mock = od_mockgraph::MockGraph::start().await;
    for i in 0..30 {
        mock.put_file(&format!("/Big/f{i:02}.txt"), b"x");
    }
    let h = Harness::start_with(mock, None, |c| c.mass_delete_threshold = 25).await;
    let big = h.path("Big");
    fs(move || {
        for e in std::fs::read_dir(&big).unwrap() {
            std::fs::remove_file(e.unwrap().path()).unwrap();
        }
    })
    .await;
    let e = h.engine.clone();
    wait_for("deletes held", 10, move || e.held_deletes_count() == 30).await;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    assert_eq!(h.mock.list("/Big").len(), 30, "nothing deleted online while held");

    h.engine.resolve_mass_delete(false).unwrap();
    let big = h.path("Big");
    let n = fs(move || std::fs::read_dir(&big).unwrap().count()).await;
    assert_eq!(n, 30, "Restore brings the files back");
    assert_eq!(h.mock.list("/Big").len(), 30);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trash_invalid_names_and_case_clashes() {
    let mock = od_mockgraph::MockGraph::start().await;
    mock.put_file("/doc.txt", b"to be trashed");
    let h = Harness::start_with(mock, None, |_| {}).await;

    // "Move to Trash" (into the local-only .Trash-UID folder) deletes online.
    let root = h.mnt.clone();
    fs(move || {
        std::fs::create_dir_all(root.join(".Trash-1000/files")).unwrap();
        std::fs::rename(root.join("doc.txt"), root.join(".Trash-1000/files/doc.txt")).unwrap();
    })
    .await;
    let m = h.mock.clone();
    wait_for("trashed file deleted online", 10, move || !m.exists("/doc.txt")).await;
    assert!(!h.mock.exists("/.Trash-1000"));
    let t = h.path(".Trash-1000/files/doc.txt");
    assert_eq!(fs(move || std::fs::read(&t).unwrap()).await, b"to be trashed");

    // A name OneDrive rejects stays local and is reported as an issue.
    let bad = h.path("bad:name.txt");
    fs({
        let b = bad.clone();
        move || std::fs::write(&b, b"x")
    })
    .await
    .unwrap();
    assert_eq!(fs(move || getxattr(&bad, STATUS)).await.as_deref(), Some("error"));

    // OneDrive is case-insensitive: a second file differing only in case is refused.
    let a = h.path("Readme.md");
    fs(move || std::fs::write(&a, b"1")).await.unwrap();
    let b = h.path("README.md");
    let err = fs(move || std::fs::write(&b, b"2")).await.unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EEXIST));

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    assert!(!h.mock.exists("/bad:name.txt"));
    assert!(h.mock.exists("/Readme.md"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsynced_changes_survive_restart() {
    let h = Harness::start().await;
    h.engine.set_paused(od_engine::engine::pause::USER, true);
    let p = h.path("offline-note.txt");
    fs(move || std::fs::write(&p, b"written while paused")).await.unwrap();
    let (mock, dir) = h.stop().await;
    assert!(!mock.exists("/offline-note.txt"));

    // Restart on the same state: the pending upload goes out.
    let h = Harness::start_with(mock, Some(dir), |_| {}).await;
    let m = h.mock.clone();
    wait_for("upload after restart", 10, move || {
        m.read("/offline-note.txt").as_deref() == Some(&b"written while paused"[..])
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_deleted_online_can_be_restored() {
    let mock = od_mockgraph::MockGraph::start().await;
    mock.put_file("/Docs/contract.pdf", b"signed copy");
    let h = Harness::start_with(mock, None, |_| {}).await;
    let p = h.path("Docs/contract.pdf");
    assert_eq!(fs(move || std::fs::read(&p).unwrap()).await, b"signed copy");

    h.mock.remove("/Docs/contract.pdf");
    h.engine.refresh();
    let p = h.path("Docs/contract.pdf");
    wait_for("removed locally", 10, move || !p.exists()).await;

    let kept = h.engine.removed_items();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].rel_path, "Docs/contract.pdf");
    let (e, id) = (h.engine.clone(), kept[0].id.clone());
    let restored = fs(move || e.restore_removed(&id)).await.unwrap();
    assert!(restored.ends_with("Docs/contract.pdf"));
    let m = h.mock.clone();
    wait_for("uploaded again", 10, move || m.read("/Docs/contract.pdf").as_deref() == Some(&b"signed copy"[..])).await;
    assert!(h.engine.removed_items().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_files_stream_and_seek() {
    use std::io::{Read, Seek, SeekFrom};
    let size = 40 * 1024 * 1024usize;
    let data: Vec<u8> = (0..size).map(|i| ((i / 4096) as u8) ^ (i as u8)).collect();
    let mock = od_mockgraph::MockGraph::start().await;
    mock.put_file("/Videos/holiday.mkv", &data);
    let h = Harness::start_with(mock, None, |_| {}).await;

    // Seek near the end: only the needed blocks are fetched first.
    let p = h.path("Videos/holiday.mkv");
    let off = 30 * 1024 * 1024u64 + 123;
    let chunk = fs(move || {
        let mut f = std::fs::File::open(&p).unwrap();
        f.seek(SeekFrom::Start(off)).unwrap();
        let mut buf = vec![0u8; 65536];
        f.read_exact(&mut buf).unwrap();
        buf
    })
    .await;
    assert_eq!(chunk, data[off as usize..off as usize + 65536]);
    assert!(h.mock.stat(|s| s.ranged_requests) >= 1);

    // The whole file reads back correctly and ends up verified and cached.
    let p = h.path("Videos/holiday.mkv");
    let all = fs(move || std::fs::read(&p).unwrap()).await;
    assert!(all == data, "streamed content differs");
    let p = h.path("Videos/holiday.mkv");
    wait_for("streamed file installed", 20, move || getxattr(&p, STATUS).as_deref() == Some("local")).await;
    let p = h.path("Videos/holiday.mkv");
    assert_eq!(fs(move || std::fs::metadata(&p).unwrap().blocks() * 512 >= size as u64).await, true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_stream_is_never_installed() {
    let size = 20 * 1024 * 1024usize;
    let mock = od_mockgraph::MockGraph::start().await;
    mock.put_file("/big.iso", &vec![7u8; size]);
    let h = Harness::start_with(mock, None, |_| {}).await;
    h.mock.fault("/_download/", od_mockgraph::FaultKind::CorruptBody, 1000);
    let p = h.path("big.iso");
    let r = fs(move || std::fs::read(&p)).await;
    // Reads either fail, or (if they completed before verification) the
    // final install is refused; either way the cache never holds bad bytes.
    let p = h.path("big.iso");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(fs(move || getxattr(&p, STATUS)).await.as_deref(), Some("online"), "{:?}", r.as_ref().map(|v| v.len()));
    h.mock.clear_faults();
    let p = h.path("big.iso");
    assert_eq!(fs(move || std::fs::read(&p).unwrap()).await.len(), size);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn online_only_media_get_server_thumbnails() {
    let thumbs = tempfile::tempdir().unwrap();
    let root = thumbs.path().to_path_buf();
    let mock = od_mockgraph::MockGraph::start().await;
    mock.put_file("/Photos/beach.jpg", &vec![1u8; 300_000]);
    mock.put_file("/Photos/scan.pdf", &vec![2u8; 1000]);
    let r2 = root.clone();
    let h = Harness::start_with(mock, None, move |c| c.thumbnail_root = r2).await;

    // A file manager lists the folder.
    let dir = h.path("Photos");
    fs(move || std::fs::read_dir(&dir).unwrap().count()).await;

    let uri = od_engine::thumbs::glib_file_uri(&h.path("Photos/beach.jpg"));
    let name = format!("{}.png", od_engine::thumbs::md5_hex(&uri));
    let (large, xl) = (root.join("large").join(&name), root.join("x-large").join(&name));
    let (l2, x2) = (large.clone(), xl.clone());
    wait_for("thumbnails written", 10, move || l2.exists() && x2.exists()).await;
    let img = image::open(&large).unwrap();
    assert_eq!((img.width(), img.height()), (256, 192), "longest edge 256, aspect kept");
    assert_eq!(h.mock.stat(|s| s.downloads), 0, "no file was downloaded to make previews");

    // No server preview for the PDF: a "failed" marker stops thumbnailers…
    let pdf_uri = od_engine::thumbs::glib_file_uri(&h.path("Photos/scan.pdf"));
    let fail = root.join("fail/gnome-thumbnail-factory").join(format!("{}.png", od_engine::thumbs::md5_hex(&pdf_uri)));
    let f2 = fail.clone();
    wait_for("fail marker", 10, move || f2.exists()).await;
    // …until the file is opened; then GNOME may thumbnail the local copy.
    let p = h.path("Photos/scan.pdf");
    fs(move || std::fs::read(&p).unwrap()).await;
    let f3 = fail.clone();
    wait_for("fail marker removed", 10, move || !f3.exists()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_notifications_bring_changes_immediately() {
    // Polling every 10 minutes: only the push channel can explain a quick update.
    let h = Harness::start_with(od_mockgraph::MockGraph::start().await, None, |c| {
        c.poll_interval = std::time::Duration::from_secs(600);
    })
    .await;
    let e = h.engine.clone();
    wait_for("push channel connected", 10, move || e.push_connected()).await;
    h.mock.put_file("/Shared notes.txt", b"typed on the web");
    let p = h.path("Shared notes.txt");
    wait_for("change pushed and pulled", 8, move || p.exists()).await;
}
