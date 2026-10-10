//! Refresh-token storage: the Secret Service (GNOME Keyring / oo7) when
//! available, otherwise a 0600 file in the account's data directory.
//! Tokens are never logged.

use od_graph::TokenSet;
use std::path::PathBuf;

const ATTR_APP: &str = "application";
const ATTR_ACCOUNT: &str = "account";

fn file_path(account_id: &str) -> PathBuf {
    od_core::paths::account_dir(account_id).join("token.json")
}

fn attrs(account_id: &str) -> [(&'static str, String); 2] {
    [(ATTR_APP, od_core::APP_ID.to_string()), (ATTR_ACCOUNT, account_id.to_string())]
}

/// Set once the Secret Service turned out to be missing, so later token
/// saves don't wait for it again.
static KEYRING_UNAVAILABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

async fn keyring() -> Option<oo7::Keyring> {
    use std::sync::atomic::Ordering;
    if KEYRING_UNAVAILABLE.load(Ordering::Relaxed) {
        return None;
    }
    let k = keyring_inner().await;
    if k.is_none() {
        KEYRING_UNAVAILABLE.store(true, Ordering::Relaxed);
    }
    k
}

async fn keyring_inner() -> Option<oo7::Keyring> {
    match tokio::time::timeout(std::time::Duration::from_secs(5), oo7::Keyring::new()).await {
        Ok(Ok(k)) => {
            if let Ok(true) = k.is_locked().await {
                // Triggers the desktop's unlock prompt if needed.
                let _ = k.unlock().await;
            }
            Some(k)
        }
        Ok(Err(e)) => {
            tracing::warn!("secret service unavailable ({e}); using a private file for tokens");
            None
        }
        Err(_) => {
            tracing::warn!("secret service did not answer; using a private file for tokens");
            None
        }
    }
}

pub async fn save(account_id: &str, label_email: &str, tokens: &TokenSet) -> anyhow::Result<()> {
    let json = serde_json::to_string(tokens)?;
    if let Some(k) = keyring().await {
        let label = format!("Linux OneDrive ({label_email})");
        match k.create_item(&label, &attrs(account_id), json.as_str(), true).await {
            Ok(()) => {
                let _ = std::fs::remove_file(file_path(account_id));
                return Ok(());
            }
            Err(e) => tracing::warn!("could not store the token in the keyring: {e}"),
        }
    }
    save_file(account_id, &json)
}

fn save_file(account_id: &str, json: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let p = file_path(account_id);
    if let Some(dir) = p.parent() {
        od_core::paths::ensure_private_dir(dir)?;
    }
    let tmp = p.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(json.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

pub async fn load(account_id: &str) -> anyhow::Result<Option<TokenSet>> {
    if let Some(k) = keyring().await
        && let Ok(items) = k.search_items(&attrs(account_id)).await
        && let Some(item) = items.first()
    {
        let secret = item.secret().await?;
        return Ok(Some(serde_json::from_slice(secret.as_bytes())?));
    }
    match std::fs::read(file_path(account_id)) {
        Ok(b) => Ok(Some(serde_json::from_slice(&b)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub async fn delete(account_id: &str) {
    if let Some(k) = keyring().await {
        let _ = k.delete(&attrs(account_id)).await;
    }
    let _ = std::fs::remove_file(file_path(account_id));
}

/// Synchronous save used from the token manager's persist hook (runs the
/// async save on the current runtime without blocking the caller).
pub fn persist_hook(account_id: String, email: String) -> od_graph::auth::PersistFn {
    std::sync::Arc::new(move |t: &TokenSet| {
        let (id, email, t) = (account_id.clone(), email.clone(), t.clone());
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                if let Err(e) = save(&id, &email, &t).await {
                    tracing::error!("failed to save refreshed tokens: {e}");
                }
            });
        }
    })
}
