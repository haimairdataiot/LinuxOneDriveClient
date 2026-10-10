//! Shortcuts to shared folders ("Add shortcut to My files") and shared
//! folders on work or school accounts (no delta on folders: full listings).

mod common;

use common::{Harness, fs, wait_for};
use od_core::types::DriveType;
use std::collections::HashMap;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shortcut_shows_as_link_and_syncs_through_its_own_engine() {
    let mock = od_mockgraph::MockGraph::start().await;
    mock.add_drive("friend01", "personal", "Friend's OneDrive", None);
    let trip = mock.mkdir_in("friend01", "Trip");
    mock.put_file_in("friend01", "Trip/plan.txt", b"day 1");
    let main = Harness::start_with(mock.clone(), None, |_| {}).await;
    mock.add_shortcut("Trip", "friend01", &trip);
    main.engine.pull_once().await.unwrap();
    let sc = main.engine.shortcuts();
    assert_eq!(sc.len(), 1);
    assert_eq!(
        (sc[0].name.as_str(), sc[0].drive_id.as_str(), sc[0].item_id.as_str()),
        ("Trip", "friend01", trip.as_str())
    );

    let shared = Harness::start_scoped(mock.clone(), "friend01", &trip, DriveType::Personal, false).await;
    main.engine.set_shortcut_targets(HashMap::from([(sc[0].id.clone(), shared.mnt.clone())]));

    // The link leads into the shared folder's own mount.
    let (link, target) = (main.path("Trip"), shared.mnt.clone());
    let (l2, t2) = (link.clone(), target.clone());
    fs(move || {
        assert!(std::fs::symlink_metadata(&l2).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_link(&l2).unwrap(), t2);
        assert_eq!(std::fs::read(l2.join("plan.txt")).unwrap(), b"day 1");
        let names: Vec<String> = std::fs::read_dir(l2.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(names.contains(&"Trip".to_string()));
        // The name is taken.
        assert!(std::fs::create_dir(&l2).is_err());
    })
    .await;

    // Writes through the link go to the shared folder, not the user's drive.
    let l2 = link.clone();
    fs(move || std::fs::write(l2.join("notes.txt"), b"bring a map").unwrap()).await;
    let m = mock.clone();
    wait_for("upload into the shared folder", 15, move || {
        m.read_in("friend01", "Trip/notes.txt").as_deref() == Some(b"bring a map".as_slice())
    })
    .await;
    assert!(!mock.exists("/notes.txt") && !mock.exists("/Trip/notes.txt"), "nothing lands in the user's drive");

    // Changes by the owner arrive through the shortcut's own delta.
    mock.put_file_in("friend01", "Trip/plan.txt", b"day 2");
    shared.engine.pull_once().await.unwrap();
    let l2 = link.clone();
    wait_for("owner's edit visible through the link", 10, move || {
        std::fs::read(l2.join("plan.txt")).map(|b| b == b"day 2").unwrap_or(false)
    })
    .await;

    // Renaming the link renames the shortcut; deleting it removes only the shortcut.
    let (l2, renamed) = (link.clone(), main.path("Holiday"));
    let r2 = renamed.clone();
    fs(move || std::fs::rename(&l2, &r2).unwrap()).await;
    assert!(mock.exists("/Holiday") && !mock.exists("/Trip"));
    assert_eq!(main.engine.shortcuts()[0].name, "Holiday");
    let r2 = renamed.clone();
    fs(move || std::fs::remove_file(&r2).unwrap()).await;
    assert!(!mock.exists("/Holiday"));
    assert!(main.engine.shortcuts().is_empty());
    assert_eq!(
        mock.read_in("friend01", "Trip/plan.txt").as_deref(),
        Some(b"day 2".as_slice()),
        "shared content intact"
    );
    let r2 = renamed.clone();
    fs(move || assert!(std::fs::symlink_metadata(&r2).is_err())).await;

    // A shortcut deleted online disappears after the next round too.
    let id = mock.add_shortcut("Trip", "friend01", &trip);
    main.engine.pull_once().await.unwrap();
    assert_eq!(main.engine.shortcuts().len(), 1);
    assert!(mock.state.lock().unwrap().items.contains_key(&id));
    mock.remove("/Trip");
    main.engine.pull_once().await.unwrap();
    assert!(main.engine.shortcuts().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn business_shared_folder_syncs_by_listing() {
    let mock = od_mockgraph::MockGraph::start().await;
    mock.set_business(true);
    mock.add_drive("teamdrv", "business", "Ana's OneDrive", None);
    let proj = mock.mkdir_in("teamdrv", "Projects");
    mock.put_file_in("teamdrv", "Projects/a.txt", b"alpha");
    mock.put_file_in("teamdrv", "Projects/sub/b.txt", b"beta");
    let delta_before = mock.stat(|s| s.delta_calls);
    let h = Harness::start_scoped(mock.clone(), "teamdrv", &proj, DriveType::Business, true).await;
    assert_eq!(mock.stat(|s| s.delta_calls), delta_before, "no delta on a business folder");
    let p = h.mnt.clone();
    fs(move || {
        assert_eq!(std::fs::read(p.join("a.txt")).unwrap(), b"alpha");
        assert_eq!(std::fs::read(p.join("sub/b.txt")).unwrap(), b"beta");
    })
    .await;

    // Remote changes, including deletes, show up after a listing round.
    mock.put_file_in("teamdrv", "Projects/c.txt", b"gamma");
    mock.remove_in("teamdrv", "Projects/a.txt");
    h.engine.pull_once().await.unwrap();
    let p = h.mnt.clone();
    fs(move || {
        assert_eq!(std::fs::read(p.join("c.txt")).unwrap(), b"gamma");
        assert!(!p.join("a.txt").exists());
    })
    .await;

    // Local changes upload into the shared folder.
    let p = h.mnt.clone();
    fs(move || std::fs::write(p.join("sub/new.txt"), b"from linux").unwrap()).await;
    let m = mock.clone();
    wait_for("upload into the business shared folder", 15, move || {
        m.read_in("teamdrv", "Projects/sub/new.txt").as_deref() == Some(b"from linux".as_slice())
    })
    .await;
    // The next listing doesn't treat the uploaded file as new or deleted.
    h.engine.pull_once().await.unwrap();
    let p = h.mnt.clone();
    fs(move || assert_eq!(std::fs::read(p.join("sub/new.txt")).unwrap(), b"from linux")).await;

    // A listing that fails part-way applies nothing (no false deletes).
    mock.fail("/children", 500, 50);
    assert!(h.engine.pull_once().await.is_err());
    mock.clear_faults();
    let p = h.mnt.clone();
    fs(move || assert!(p.join("c.txt").exists() && p.join("sub/b.txt").exists())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn falls_back_to_listing_when_folder_delta_is_refused() {
    let mock = od_mockgraph::MockGraph::start().await;
    mock.set_business(true);
    mock.add_drive("teamdrv", "business", "Ana's OneDrive", None);
    let proj = mock.mkdir_in("teamdrv", "Projects");
    mock.put_file_in("teamdrv", "Projects/a.txt", b"alpha");
    // Not told to list: it tries delta, is refused, and lists by itself.
    let h = Harness::start_scoped(mock.clone(), "teamdrv", &proj, DriveType::Business, false).await;
    let p = h.mnt.clone();
    fs(move || assert_eq!(std::fs::read(p.join("a.txt")).unwrap(), b"alpha")).await;
    let delta_after_start = mock.stat(|s| s.delta_calls);
    mock.put_file_in("teamdrv", "Projects/b.txt", b"beta");
    h.engine.pull_once().await.unwrap();
    assert_eq!(mock.stat(|s| s.delta_calls), delta_after_start, "delta isn't tried again");
    let p = h.mnt.clone();
    fs(move || assert_eq!(std::fs::read(p.join("b.txt")).unwrap(), b"beta")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn business_folder_uses_delta_when_offered_and_takes_over_a_listed_state() {
    let mock = od_mockgraph::MockGraph::start().await;
    mock.set_business(true);
    mock.set_business_folder_delta(true);
    mock.add_drive("teamdrv", "business", "Ana's OneDrive", None);
    let proj = mock.mkdir_in("teamdrv", "Projects");
    mock.put_file_in("teamdrv", "Projects/a.txt", b"alpha");
    mock.put_file_in("teamdrv", "Projects/sub/b.txt", b"beta");
    mock.put_file_in("teamdrv", "Other/outside.txt", b"not shared");

    // An earlier version built this folder's state by listing it.
    let h = Harness::start_scoped(mock.clone(), "teamdrv", &proj, DriveType::Business, true).await;
    let p = h.mnt.clone();
    fs(move || {
        assert_eq!(std::fs::read(p.join("a.txt")).unwrap(), b"alpha"); // downloaded
        std::fs::write(p.join("mine.txt"), b"local work").unwrap();
    })
    .await;
    let m = mock.clone();
    wait_for("upload", 15, move || m.read_in("teamdrv", "Projects/mine.txt").is_some()).await;
    let (mock, dir) = h.stop().await;

    // Now delta is used: one enumeration, nothing downloaded again, nothing removed.
    let downloads = mock.stat(|s| s.downloads);
    let deletes = mock.stat(|s| s.deletes);
    let h = Harness::start_scoped_in(mock.clone(), Some(dir), "teamdrv", &proj, DriveType::Business, false).await;
    assert!(mock.stat(|s| s.delta_calls) > 0);
    let p = h.mnt.clone();
    fs(move || {
        assert_eq!(std::fs::read(p.join("a.txt")).unwrap(), b"alpha");
        assert_eq!(std::fs::read(p.join("mine.txt")).unwrap(), b"local work");
        assert!(p.join("sub/b.txt").exists());
        assert!(!p.join("outside.txt").exists() && !p.join("Other").exists(), "only the shared folder");
    })
    .await;
    assert_eq!(mock.stat(|s| s.downloads), downloads, "kept the downloaded copies");
    assert_eq!(mock.stat(|s| s.deletes), deletes);

    // From here on, changes cost one small request instead of a listing.
    let before = mock.stat(|s| s.requests);
    mock.put_file_in("teamdrv", "Projects/c.txt", b"gamma");
    mock.remove_in("teamdrv", "Projects/sub/b.txt");
    h.engine.pull_once().await.unwrap();
    assert!(mock.stat(|s| s.requests) - before <= 2, "incremental round");
    let p = h.mnt.clone();
    fs(move || {
        assert_eq!(std::fs::read(p.join("c.txt")).unwrap(), b"gamma");
        assert!(!p.join("sub/b.txt").exists());
    })
    .await;
}
