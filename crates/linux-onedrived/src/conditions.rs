//! Environment conditions that pause or resume syncing: metered network,
//! connectivity (NetworkManager) and power-saver mode (power-profiles-daemon).

use futures::StreamExt;

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManager {
    /// NMMetered: 0 unknown, 1 yes, 2 no, 3 guess-yes, 4 guess-no.
    #[zbus(property)]
    fn metered(&self) -> zbus::Result<u32>;
    /// NMConnectivityState: 0 unknown, 1 none, 2 portal, 3 limited, 4 full.
    #[zbus(property)]
    fn connectivity(&self) -> zbus::Result<u32>;
}

#[zbus::proxy(
    interface = "net.hadess.PowerProfiles",
    default_service = "net.hadess.PowerProfiles",
    default_path = "/net/hadess/PowerProfiles"
)]
trait PowerProfiles {
    #[zbus(property)]
    fn active_profile(&self) -> zbus::Result<String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Conditions {
    pub metered: bool,
    pub power_saver: bool,
    /// None when unknown (no NetworkManager).
    pub online: Option<bool>,
}

fn metered(v: u32) -> bool {
    matches!(v, 1 | 3)
}

fn online(v: u32) -> Option<bool> {
    match v {
        0 => None,
        1 => Some(false),
        _ => Some(true),
    }
}

/// Watch the system bus and call `on_change` with every new state.
pub async fn watch(on_change: impl Fn(Conditions) + Send + Sync + 'static) {
    let Ok(sys) = zbus::Connection::system().await else {
        tracing::info!("no system bus: metered/power-saver detection disabled");
        return;
    };
    let nm = NetworkManagerProxy::new(&sys).await.ok();
    let pp = PowerProfilesProxy::new(&sys).await.ok();
    let mut cur = Conditions::default();
    if let Some(nm) = &nm {
        cur.metered = nm.metered().await.map(metered).unwrap_or(false);
        cur.online = nm.connectivity().await.ok().and_then(online);
    }
    if let Some(pp) = &pp {
        cur.power_saver = pp.active_profile().await.map(|p| p == "power-saver").unwrap_or(false);
    }
    on_change(cur);

    let mut metered_s = match &nm {
        Some(n) => Some(n.receive_metered_changed().await),
        None => None,
    };
    let mut conn_s = match &nm {
        Some(n) => Some(n.receive_connectivity_changed().await),
        None => None,
    };
    let mut power_s = match &pp {
        Some(p) => Some(p.receive_active_profile_changed().await),
        None => None,
    };
    loop {
        let before = cur;
        tokio::select! {
            Some(c) = async { match metered_s.as_mut() { Some(s) => s.next().await, None => std::future::pending().await } } => {
                if let Ok(v) = c.get().await { cur.metered = metered(v); }
            }
            Some(c) = async { match conn_s.as_mut() { Some(s) => s.next().await, None => std::future::pending().await } } => {
                if let Ok(v) = c.get().await { cur.online = online(v); }
            }
            Some(c) = async { match power_s.as_mut() { Some(s) => s.next().await, None => std::future::pending().await } } => {
                if let Ok(v) = c.get().await { cur.power_saver = v == "power-saver"; }
            }
            else => break,
        }
        if cur != before {
            tracing::info!(?cur, "conditions changed");
            on_change(cur);
        }
    }
}
