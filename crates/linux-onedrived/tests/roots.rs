//! Shortcuts to shared folders and SharePoint libraries through the daemon:
//! links in the OneDrive folder, path routing, Settings' locations, the
//! browser's Sync button (odopen://) and Stop sync.

mod common;

use common::wait_until;
use od_core::dbus::{LibraryInfo, PathInfo, SiteInfo, SyncLocation};
use od_mockgraph::MockGraph;
use std::path::PathBuf;

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shortcuts_and_libraries() {
    let mock = MockGraph::start().await;
    mock.set_business(true);
    // A colleague's folder, added with "Add shortcut to My files".
    mock.add_drive("anadrive", "business", "Ana's OneDrive", None);
    let proj = mock.mkdir_in("anadrive", "Projects");
    mock.put_file_in("anadrive", "Projects/brief.txt", b"the brief");
    let placeholder = mock.add_shortcut("Projects", "anadrive", &proj);
    // A SharePoint document library.
    mock.add_drive("libdrive", "documentLibrary", "Documents", Some(("site-mkt", "Marketing")));
    mock.put_file_in("libdrive", "Plan.txt", b"q4 plan");

    let s = common::start(mock.clone(), |_| {}).await;
    let link = s.mount.join("Projects");
    let l = link.clone();
    wait_until("the shortcut's link", 30, || {
        let l = l.clone();
        async move {
            blocking(move || {
                std::fs::symlink_metadata(&l).is_ok_and(|m| m.file_type().is_symlink())
                    && std::fs::read(l.join("brief.txt")).is_ok_and(|b| b == b"the brief")
            })
            .await
        }
    })
    .await;

    // Status and sharing work on paths through the link.
    let brief = link.join("brief.txt").to_string_lossy().into_owned();
    let info: Vec<PathInfo> = serde_json::from_str(&s.mgr.paths_info(&[brief.as_str()]).await.unwrap()).unwrap();
    assert_eq!(info[0].account, s.acct.inner().path().as_str());
    assert!(!info[0].status.is_empty(), "the shortcut's engine answers for it");
    assert!(s.acct.copy_link(&brief).await.unwrap().starts_with("https://"));

    // Writes through the link land in the shared folder.
    let l = link.clone();
    blocking(move || std::fs::write(l.join("notes.txt"), b"from linux").unwrap()).await;
    wait_until("upload into the shared folder", 30, || async {
        mock.read_in("anadrive", "Projects/notes.txt").as_deref() == Some(b"from linux".as_slice())
    })
    .await;

    let locs: Vec<SyncLocation> = serde_json::from_str(&s.acct.list_locations().await.unwrap()).unwrap();
    assert_eq!(locs.len(), 1);
    assert_eq!((locs[0].kind.as_str(), locs[0].title.as_str()), ("shortcut", "Projects"));
    assert_eq!(PathBuf::from(&locs[0].local_path), link);

    // Sync a library chosen in Settings.
    let sites: Vec<SiteInfo> = serde_json::from_str(&s.acct.search_sites("mark").await.unwrap()).unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].name, "Marketing");
    let libs: Vec<LibraryInfo> = serde_json::from_str(&s.acct.site_libraries(&sites[0].id).await.unwrap()).unwrap();
    assert_eq!(libs.len(), 1);
    assert!(!libs[0].synced);
    let lib_path = PathBuf::from(s.acct.add_library(&sites[0].id, &libs[0].drive_id).await.unwrap());
    assert_eq!(lib_path, s.home.join("SharePoint/Marketing - Documents"));
    let p = lib_path.clone();
    wait_until("library content", 30, || {
        let p = p.clone();
        async move { blocking(move || std::fs::read(p.join("Plan.txt")).is_ok_and(|b| b == b"q4 plan")).await }
    })
    .await;
    let libs: Vec<LibraryInfo> = serde_json::from_str(&s.acct.site_libraries(&sites[0].id).await.unwrap()).unwrap();
    assert!(libs[0].synced);

    // The browser's Sync button for the same library: already synced, same folder.
    let list = mock.list_id("libdrive");
    let url = format!(
        "odopen://sync/?userEmail=mock.user%40example.com&siteId=%7B11111111-1111%7D&webId=%7B22222222-2222%7D\
         &listId=%7B{list}%7D&webUrl=https%3A%2F%2Fcontoso.example%2Fsites%2FMarketing&webTitle=Marketing\
         &listTitle=Documents"
    );
    assert_eq!(PathBuf::from(s.mgr.open_sync_link(&url).await.unwrap()), lib_path);
    assert!(s.mgr.open_sync_link("odopen://sync/?webTitle=x").await.is_err());

    // Stop sync for the library: unmounted and forgotten.
    let locs: Vec<SyncLocation> = serde_json::from_str(&s.acct.list_locations().await.unwrap()).unwrap();
    assert_eq!(locs.len(), 2);
    let lib = locs.iter().find(|l| l.kind == "library").unwrap();
    s.acct.remove_location(&lib.key).await.unwrap();
    let p = lib_path.clone();
    assert!(blocking(move || std::fs::read_dir(&p).map(|mut d| d.next().is_none()).unwrap_or(true)).await);
    let cfg = std::fs::read_to_string(s.home.join(".config/linux-onedrive/config.toml")).unwrap();
    assert!(!cfg.contains("libdrive"), "the library is forgotten");

    // Remove the shortcut from Settings: the link goes, the shared folder stays.
    let sc = locs.iter().find(|l| l.kind == "shortcut").unwrap();
    s.acct.remove_location(&sc.key).await.unwrap();
    assert!(mock.state.lock().unwrap().items.get(&placeholder).is_none_or(|i| i.deleted));
    let l = link.clone();
    assert!(blocking(move || std::fs::symlink_metadata(&l).is_err()).await);
    assert_eq!(mock.read_in("anadrive", "Projects/brief.txt").as_deref(), Some(b"the brief".as_slice()));
    let locs: Vec<SyncLocation> = serde_json::from_str(&s.acct.list_locations().await.unwrap()).unwrap();
    assert!(locs.is_empty());
}
