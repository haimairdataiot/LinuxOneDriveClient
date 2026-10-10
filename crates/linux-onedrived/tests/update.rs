//! App updates through the daemon: the check against the (emulated) GitHub
//! releases API, and the checksum-verified download.

mod common;

use od_core::update::UpdateInfo;
use od_mockgraph::{MockGraph, MockRelease};
use std::path::PathBuf;

fn release(tag: &str, package: &[u8]) -> MockRelease {
    let version = tag.trim_start_matches('v');
    MockRelease {
        tag: tag.into(),
        notes: "- Faster sync\n- Fixes".into(),
        asset_name: format!("linux-onedrive_{version}-1_{}.deb", od_core::update::deb_arch()),
        package: package.to_vec().into(),
        with_checksums: true,
        wrong_checksum: false,
        prerelease: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn update_check_and_verified_download() {
    let mock = MockGraph::start().await;
    let s = common::start(mock.clone(), |_| {}).await;
    let check = |force: bool| {
        let mgr = s.mgr.clone();
        async move { serde_json::from_str::<UpdateInfo>(&mgr.check_for_update(force).await.unwrap()).unwrap() }
    };

    // Nothing published: not an update, and the reason is given.
    let info = check(true).await;
    assert!(!info.available && !info.error.is_empty());
    assert_eq!(info.current, od_core::VERSION);
    assert_eq!(s.mgr.update_version().await.unwrap(), "");
    assert!(s.mgr.download_update().await.is_err());

    // The running version (or an older one) is published: up to date.
    mock.set_release(Some(release(&format!("v{}", od_core::VERSION), b"same")));
    let info = check(true).await;
    assert!(!info.available && info.error.is_empty());
    assert_eq!(info.latest, od_core::VERSION);
    mock.set_release(Some(release("v0.0.1", b"old")));
    assert!(!check(true).await.available);
    assert!(s.mgr.download_update().await.is_err(), "nothing to download when up to date");

    // A newer version: offered, with notes; a pre-release is not.
    let package = vec![7u8; 300_000];
    let mut pre = release("v99.1.0", &package);
    pre.prerelease = true;
    mock.set_release(Some(pre));
    assert!(!check(true).await.available);
    mock.set_release(Some(release("v99.1.0", &package)));
    let info = check(true).await;
    assert!(info.available && info.installable);
    assert_eq!(info.latest, "99.1.0");
    assert!(info.notes.contains("Faster sync"));
    assert_eq!(s.mgr.update_version().await.unwrap(), "99.1.0");
    // A recent answer is reused unless a fresh check is forced.
    mock.set_release(Some(release("v99.2.0", &package)));
    assert_eq!(check(false).await.latest, "99.1.0");
    assert_eq!(check(true).await.latest, "99.2.0");

    // Download: the file arrives intact, in the cache, and a second request reuses it.
    let path = PathBuf::from(s.mgr.download_update().await.unwrap());
    assert!(path.starts_with(s.home.join(".cache")));
    assert!(path.to_string_lossy().ends_with(".deb"));
    assert_eq!(std::fs::read(&path).unwrap(), package);
    assert_eq!(PathBuf::from(s.mgr.download_update().await.unwrap()), path);

    // A download that doesn't match the published checksum is refused and removed.
    let mut bad = release("v99.3.0", b"tampered package");
    bad.wrong_checksum = true;
    mock.set_release(Some(bad));
    assert!(check(true).await.available);
    let err = s.mgr.download_update().await.unwrap_err().to_string();
    assert!(err.contains("checksum"), "{err}");
    let dir = path.parent().unwrap();
    let left: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(left.is_empty(), "no unverified package is kept: {left:?}");

    // A release without a checksum file can't be installed by the app.
    let mut nosum = release("v99.4.0", b"x");
    nosum.with_checksums = false;
    mock.set_release(Some(nosum));
    let info = check(true).await;
    assert!(info.available && !info.installable);
    assert!(s.mgr.download_update().await.is_err());
}
