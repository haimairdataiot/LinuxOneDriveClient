//! The real daemon binary on a private D-Bus session bus, against the mock
//! OneDrive: sign-in, setup, sync, pause, settings and file-manager queries,
//! all through the same proxies the GUI and CLI use.

use od_core::dbus::{AccountProxy, ManagerProxy};
use od_mockgraph::MockGraph;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Bus {
    child: Child,
    address: String,
}

impl Bus {
    fn start() -> Bus {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon");
        use std::io::BufRead;
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        Bus { child, address: line.trim().to_string() }
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

struct DaemonProc {
    child: Child,
    mount: std::path::PathBuf,
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = Command::new("fusermount3").arg("-uz").arg(&self.mount).status();
    }
}

async fn wait_until<F, Fut>(what: &str, secs: u64, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !f().await {
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sign_in_setup_sync_over_dbus() {
    let mock = MockGraph::start().await;
    mock.put_file("/Documents/welcome.txt", b"hello from the cloud");
    let tmp = tempfile::Builder::new().prefix("lod-daemon-").tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".config/linux-onedrive")).unwrap();
    std::fs::write(
        home.join(".config/linux-onedrive/config.toml"),
        "[general]\nclient_id = \"00000000-test\"\npoll_interval_secs = 15\n",
    )
    .unwrap();
    let bus = Bus::start();

    let child = Command::new(env!("CARGO_BIN_EXE_linux-onedrived"))
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .env("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("LINUX_ONEDRIVE_GRAPH_URL", mock.base_url())
        .env("LINUX_ONEDRIVE_LOGIN_URL", mock.base_url())
        .env("RUST_LOG", "warn")
        .env_remove("NOTIFY_SOCKET")
        .env_remove("JOURNAL_STREAM")
        .spawn()
        .expect("spawn daemon");
    let mut daemon = DaemonProc { child, mount: home.join("OneDrive") };

    let conn = zbus::connection::Builder::address(bus.address.as_str()).unwrap().build().await.unwrap();
    let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
    wait_until("daemon on the bus", 15, || async {
        dbus.name_has_owner(od_core::dbus::BUS_NAME.try_into().unwrap()).await.unwrap_or(false)
    })
    .await;
    // Uncached proxies: every property read asks the daemon (as the CLI does).
    let mgr = ManagerProxy::builder(&conn).cache_properties(zbus::proxy::CacheProperties::No).build().await.unwrap();
    assert_eq!(mgr.state().await.unwrap(), "signed-out");
    assert!(!mgr.needs_client_id().await.unwrap());

    // Embedded sign-in: the GUI would show `url` in a web view and intercept `redirect`.
    let (url, redirect) = mgr.begin_sign_in("mock.user@example.com", "embedded").await.unwrap();
    assert!(url.contains("login_hint=mock.user%40example.com"));
    assert!(redirect.ends_with("/common/oauth2/nativeclient"));
    let state = url::Url::parse(&url).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();
    let path = mgr.complete_sign_in(&format!("{redirect}?code=abc&state={state}")).await.unwrap();
    let acct = AccountProxy::builder(&conn)
        .path(path.clone())
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap();
    assert_eq!(acct.email().await.unwrap(), "mock.user@example.com");
    assert!(acct.needs_setup().await.unwrap());
    assert_eq!(mgr.accounts().await.unwrap().len(), 1);
    let mount = std::path::PathBuf::from(acct.mount_point().await.unwrap());
    assert_eq!(mount, home.join("OneDrive"));

    // Tokens were stored privately (no keyring on this bus).
    let id = acct.id().await.unwrap();
    let token_file = home.join(".local/share/linux-onedrive/accounts").join(&id).join("token.json");
    assert!(token_file.exists());
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&token_file).unwrap().permissions().mode() & 0o777, 0o600);

    // Finish setup: mount + sync.
    acct.start().await.unwrap();
    let f = mount.join("Documents/welcome.txt");
    wait_until("remote file visible", 20, || {
        let f = f.clone();
        async move { tokio::task::spawn_blocking(move || f.exists()).await.unwrap() }
    })
    .await;
    let f2 = f.clone();
    let text = tokio::task::spawn_blocking(move || std::fs::read_to_string(f2).unwrap()).await.unwrap();
    assert_eq!(text, "hello from the cloud");

    // File-manager query.
    let info = mgr.paths_info(&[f.to_str().unwrap()]).await.unwrap();
    let info: Vec<od_core::dbus::PathInfo> = serde_json::from_str(&info).unwrap();
    assert_eq!(info[0].status, "local");
    assert_eq!(info[0].account, path.as_str());

    // Upload through the mount; activity is reported.
    let newf = mount.join("Documents/from-linux.txt");
    tokio::task::spawn_blocking(move || std::fs::write(newf, b"uploaded via daemon")).await.unwrap().unwrap();
    let m = mock.clone();
    wait_until("upload", 20, || {
        let m = m.clone();
        async move { m.read("/Documents/from-linux.txt").as_deref() == Some(&b"uploaded via daemon"[..]) }
    })
    .await;
    let activity: Vec<od_core::dbus::ActivityEntry> =
        serde_json::from_str(&acct.recent_activity(20).await.unwrap()).unwrap();
    assert!(
        activity.iter().any(|a| a.name == "from-linux.txt" && a.kind == od_core::types::ActivityKind::Uploaded),
        "{activity:?}"
    );

    // Pause / resume and settings round-trip.
    mgr.pause(3600).await.unwrap();
    assert!(mgr.paused_until().await.unwrap() > 0);
    assert_eq!(acct.state().await.unwrap(), "paused");
    mgr.resume().await.unwrap();
    assert_eq!(mgr.paused_until().await.unwrap(), 0);
    mgr.set_settings(r#"{"network": {"upload_limit_kbps": 2048}}"#).await.unwrap();
    let s: od_core::dbus::SettingsDoc = serde_json::from_str(&mgr.get_settings().await.unwrap()).unwrap();
    assert_eq!(s.network.upload_limit_kbps, 2048);
    assert!(s.general.pause_on_metered, "untouched settings keep their values");

    // Sharing link and pin through D-Bus.
    let link = acct.create_share_link(f.to_str().unwrap(), "view", "anonymous").await.unwrap();
    assert!(link.starts_with("https://"));
    acct.set_pin_state(&[mount.join("Documents").to_str().unwrap()], "pinned").await.unwrap();

    // Clean shutdown unmounts.
    mgr.quit().await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(Some(status)) = daemon.child.try_wait() {
            assert!(status.success());
            break;
        }
        assert!(std::time::Instant::now() < deadline, "daemon did not exit");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    assert!(!mounts.contains(mount.to_str().unwrap()), "unmounted on exit");
}

/// Two sign-ins to the same account that finish at the same moment (a
/// browser sign-in left open and the built-in page, say) add it once. The
/// slow drive lookup keeps both inside the check at the same time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_sign_ins_add_the_account_once() {
    use futures::StreamExt;
    let mock = MockGraph::start().await;
    mock.set_latency("/me/drive", 1500);
    let tmp = tempfile::Builder::new().prefix("lod-twice-").tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".config/linux-onedrive")).unwrap();
    std::fs::write(home.join(".config/linux-onedrive/config.toml"), "[general]\nclient_id = \"00000000-test\"\n")
        .unwrap();
    let bus = Bus::start();
    let child = Command::new(env!("CARGO_BIN_EXE_linux-onedrived"))
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .env("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("LINUX_ONEDRIVE_GRAPH_URL", mock.base_url())
        .env("LINUX_ONEDRIVE_LOGIN_URL", mock.base_url())
        .env("RUST_LOG", "warn")
        .env_remove("NOTIFY_SOCKET")
        .env_remove("JOURNAL_STREAM")
        .spawn()
        .expect("spawn daemon");
    let _daemon = DaemonProc { child, mount: home.join("OneDrive") };
    let conn = zbus::connection::Builder::address(bus.address.as_str()).unwrap().build().await.unwrap();
    let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
    wait_until("daemon on the bus", 15, || async {
        dbus.name_has_owner(od_core::dbus::BUS_NAME.try_into().unwrap()).await.unwrap_or(false)
    })
    .await;
    let mgr = ManagerProxy::builder(&conn).cache_properties(zbus::proxy::CacheProperties::No).build().await.unwrap();
    let state_of =
        |url: &str| url::Url::parse(url).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();
    let mut finished = mgr.receive_sign_in_finished().await.unwrap();

    let (browser_url, browser_redirect) = mgr.begin_sign_in("mock.user@example.com", "browser").await.unwrap();
    let (page_url, page_redirect) = mgr.begin_sign_in("mock.user@example.com", "embedded").await.unwrap();
    let browser = async {
        let to = format!("{browser_redirect}/?code=abc&state={}", state_of(&browser_url));
        reqwest::get(to).await.unwrap();
        let signal = tokio::time::timeout(Duration::from_secs(20), finished.next()).await.unwrap().unwrap();
        let args = signal.args().unwrap();
        assert!(args.ok, "{}", args.message);
        args.account.to_string()
    };
    let page = async {
        mgr.complete_sign_in(&format!("{page_redirect}?code=abc&state={}", state_of(&page_url)))
            .await
            .unwrap()
            .to_string()
    };
    let (from_browser, from_page) = tokio::join!(browser, page);

    assert_eq!(from_browser, from_page, "both sign-ins name the same account");
    assert_eq!(mgr.accounts().await.unwrap().len(), 1);
    let config = std::fs::read_to_string(home.join(".config/linux-onedrive/config.toml")).unwrap();
    assert_eq!(config.matches("[[account]]").count(), 1, "{config}");
}
