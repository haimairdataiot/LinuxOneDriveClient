//! Standalone fake OneDrive for demos and manual GUI testing:
//!     od-mockgraph [--seed] [--business] [--release <version>] [--user <name> <email>]
//! Prints the base URL; point LINUX_ONEDRIVE_GRAPH_URL and
//! LINUX_ONEDRIVE_LOGIN_URL at it.

#[tokio::main]
async fn main() {
    let mock = od_mockgraph::MockGraph::start().await;
    if std::env::args().any(|a| a == "--seed") {
        mock.put_file("/Documents/Quarterly report.docx", &vec![1u8; 48_000]);
        mock.put_file("/Documents/Budget 2026.xlsx", &vec![2u8; 21_000]);
        mock.put_file("/Documents/Invoice 1042.pdf", &vec![8u8; 86_000]);
        mock.put_file("/Documents/Team offsite.mp4", &vec![9u8; 64_000_000]);
        mock.put_file("/Documents/Notes/meeting-notes.md", b"# Meeting notes\n");
        mock.put_file("/Pictures/Holiday/beach.jpg", &vec![3u8; 350_000]);
        mock.put_file("/Pictures/Holiday/sunset.jpg", &vec![4u8; 410_000]);
        mock.put_file("/Music/playlist.m3u", b"#EXTM3U\n");
        mock.put_file("/Getting started with OneDrive.pdf", &vec![5u8; 120_000]);
        mock.mkdir("/Desktop");
        // A friend's folder, added with "Add shortcut to My files".
        mock.add_drive("friend0001", "personal", "Sam's OneDrive", None);
        let trip = mock.mkdir_in("friend0001", "Trip planning");
        mock.put_file_in("friend0001", "Trip planning/Itinerary.docx", &vec![6u8; 30_000]);
        mock.put_file_in("friend0001", "Trip planning/Packing list.txt", b"passport\ncharger\n");
        mock.add_shortcut("Trip planning", "friend0001", &trip);
    }
    if std::env::args().any(|a| a == "--business") {
        mock.set_business(true);
        mock.add_drive("libdrive01", "documentLibrary", "Documents", Some(("site-mkt", "Marketing")));
        mock.put_file_in("libdrive01", "Campaign plan.docx", &vec![7u8; 25_000]);
        mock.add_drive("libdrive02", "documentLibrary", "Shared Documents", Some(("site-eng", "Engineering")));
    }
    let args: Vec<String> = std::env::args().collect();
    // --user <name> <email>: who is signed in.
    if let Some(i) = args.iter().position(|a| a == "--user")
        && let (Some(name), Some(email)) = (args.get(i + 1), args.get(i + 2))
    {
        mock.set_user(name, email);
    }
    // --release <version>: publish a fake app release for the update check.
    if let Some(version) = args.iter().position(|a| a == "--release").and_then(|i| args.get(i + 1)) {
        mock.set_release(Some(od_mockgraph::MockRelease {
            tag: format!("v{version}"),
            notes: "- Updates from inside the app\n- Faster sync of shared folders\n- Fixes for opening folders and Settings".into(),
            asset_name: format!("linux-onedrive_{version}-1_amd64.deb"),
            package: vec![0u8; 4096].into(),
            with_checksums: true,
            wrong_checksum: false,
            prerelease: false,
        }));
    }
    println!("{}", mock.base_url());
    tokio::signal::ctrl_c().await.ok();
}
