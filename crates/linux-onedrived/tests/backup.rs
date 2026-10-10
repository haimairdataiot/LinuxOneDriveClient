//! Folder backup ("Manage back up") through the real daemon.

mod common;

use od_core::dbus::BackupFolder;
use od_mockgraph::MockGraph;

async fn folders(s: &common::Stack) -> Vec<BackupFolder> {
    serde_json::from_str(&s.acct.list_backup_folders().await.unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn back_up_documents_and_stop() {
    let s = common::start(MockGraph::start().await, |home| {
        std::fs::create_dir_all(home.join("Documents/Taxes")).unwrap();
        std::fs::write(home.join("Documents/cv.odt"), b"my cv").unwrap();
        std::fs::write(home.join("Documents/Taxes/2025.pdf"), vec![7u8; 30_000]).unwrap();
        std::fs::write(home.join(".config/user-dirs.dirs"), "XDG_DOCUMENTS_DIR=\"$HOME/Documents\"\n").unwrap();
    })
    .await;
    let docs = s.home.join("Documents");
    let list = folders(&s).await;
    let d = list.iter().find(|f| f.key == "DOCUMENTS").unwrap();
    assert!(!d.backed_up);
    assert_eq!(d.files, 2);

    s.acct.set_folder_backup("DOCUMENTS", true).await.unwrap();
    common::wait_until("backup finished", 30, || async {
        folders(&s).await.iter().any(|f| f.key == "DOCUMENTS" && f.backed_up && !f.in_progress)
    })
    .await;
    // The old location is a link into OneDrive, apps keep working.
    assert!(docs.is_symlink());
    assert_eq!(std::fs::read_link(&docs).unwrap(), s.mount.join("Documents"));
    let docs2 = docs.clone();
    let cv = tokio::task::spawn_blocking(move || std::fs::read(docs2.join("cv.odt")).unwrap()).await.unwrap();
    assert_eq!(cv, b"my cv");
    let dirs = std::fs::read_to_string(s.home.join(".config/user-dirs.dirs")).unwrap();
    assert!(dirs.contains("XDG_DOCUMENTS_DIR=\"$HOME/OneDrive/Documents\""), "{dirs}");
    let m = s.mock.clone();
    common::wait_until("uploaded", 20, || {
        let m = m.clone();
        async move { m.read("/Documents/Taxes/2025.pdf").map(|v| v.len()) == Some(30_000) && m.exists("/Documents/cv.odt") }
    })
    .await;

    // Stop: a real folder again, files copied back, still online.
    s.acct.set_folder_backup("DOCUMENTS", false).await.unwrap();
    common::wait_until("backup stopped", 30, || async {
        folders(&s).await.iter().any(|f| f.key == "DOCUMENTS" && !f.backed_up && !f.in_progress)
    })
    .await;
    assert!(!docs.is_symlink() && docs.is_dir());
    assert_eq!(std::fs::read(docs.join("Taxes/2025.pdf")).unwrap().len(), 30_000);
    assert!(
        std::fs::read_to_string(s.home.join(".config/user-dirs.dirs"))
            .unwrap()
            .contains("XDG_DOCUMENTS_DIR=\"$HOME/Documents\"")
    );
    assert!(s.mock.exists("/Documents/cv.odt"));
}
