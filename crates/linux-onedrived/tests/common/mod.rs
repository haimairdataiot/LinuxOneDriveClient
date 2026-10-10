//! Shared harness: a private D-Bus session, the real daemon binary and the
//! mock OneDrive, with a signed-in and started account.

#![allow(dead_code)]

use od_core::dbus::{AccountProxy, BUS_NAME, ManagerProxy};
use od_mockgraph::MockGraph;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use zbus::proxy::CacheProperties;

pub struct Stack {
    pub mock: MockGraph,
    pub home: PathBuf,
    pub conn: zbus::Connection,
    pub mgr: ManagerProxy<'static>,
    pub acct: AccountProxy<'static>,
    pub mount: PathBuf,
    bus: Child,
    daemon: Child,
    _tmp: tempfile::TempDir,
}

impl Drop for Stack {
    fn drop(&mut self) {
        unsafe { libc::kill(self.daemon.id() as i32, libc::SIGTERM) };
        for _ in 0..100 {
            if let Ok(Some(_)) = self.daemon.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.daemon.kill();
        let _ = Command::new("fusermount3").arg("-uz").arg(&self.mount).status();
        let _ = self.bus.kill();
    }
}

pub async fn wait_until<F, Fut>(what: &str, secs: u64, mut f: F)
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

/// Start everything; `prepare` can populate the fake home before the daemon starts.
pub async fn start(mock: MockGraph, prepare: impl FnOnce(&std::path::Path)) -> Stack {
    let tmp = tempfile::Builder::new().prefix("lod-stack-").tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".config/linux-onedrive")).unwrap();
    std::fs::write(
        home.join(".config/linux-onedrive/config.toml"),
        "[general]\nclient_id = \"00000000-test\"\npoll_interval_secs = 15\n",
    )
    .unwrap();
    prepare(&home);

    let mut bus = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::BufRead;
    let mut address = String::new();
    std::io::BufReader::new(bus.stdout.take().unwrap()).read_line(&mut address).unwrap();
    let address = address.trim().to_string();

    let daemon = Command::new(env!("CARGO_BIN_EXE_linux-onedrived"))
        .env("DBUS_SESSION_BUS_ADDRESS", &address)
        .env("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("LINUX_ONEDRIVE_GRAPH_URL", mock.base_url())
        .env("LINUX_ONEDRIVE_LOGIN_URL", mock.base_url())
        .env("LINUX_ONEDRIVE_UPDATE_API", mock.base_url())
        .env("RUST_LOG", "warn")
        .env_remove("NOTIFY_SOCKET")
        .env_remove("JOURNAL_STREAM")
        .spawn()
        .unwrap();

    let conn = zbus::connection::Builder::address(address.as_str()).unwrap().build().await.unwrap();
    let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
    wait_until("daemon on the bus", 15, || async {
        dbus.name_has_owner(BUS_NAME.try_into().unwrap()).await.unwrap_or(false)
    })
    .await;
    let mgr = ManagerProxy::builder(&conn).cache_properties(CacheProperties::No).build().await.unwrap();
    let (url, redirect) = mgr.begin_sign_in("mock.user@example.com", "embedded").await.unwrap();
    let state = url::Url::parse(&url).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();
    let path = mgr.complete_sign_in(&format!("{redirect}?code=abc&state={state}")).await.unwrap();
    let acct =
        AccountProxy::builder(&conn).path(path).unwrap().cache_properties(CacheProperties::No).build().await.unwrap();
    acct.start().await.unwrap();
    let mount = PathBuf::from(acct.mount_point().await.unwrap());
    let m = mount.clone();
    wait_until("mounted", 15, || {
        let m = m.clone();
        async move { tokio::task::spawn_blocking(move || m.join(".trackerignore").exists()).await.unwrap() }
    })
    .await;
    Stack { mock, home, conn, mgr, acct, mount, bus, daemon, _tmp: tmp }
}
