//! Real-time change notifications (the socket.io channel OneDrive offers
//! desktop clients). A notification carries no item details; it only means
//! "something changed", so it triggers a delta round a moment later.
//! Polling stays on as a safety net, at a slower pace while connected.
//!
//! Protocol (research doc 02 §9): GET …/root/subscriptions/socketIo →
//! notificationUrl; WebSocket to wss://host/socket.io/?EIO=4&transport=websocket&<query>;
//! Engine.IO v4 (server pings "2", we answer "3"); Socket.IO namespace = the
//! URL's path ("/notifications"); events arrive as `42/notifications,[…]`.

use crate::engine::Engine;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Build the WebSocket URL and namespace from a notificationUrl.
pub fn socket_url(notification_url: &str) -> Option<(String, String)> {
    let u = url::Url::parse(notification_url).ok()?;
    let scheme = match u.scheme() {
        "https" => "wss",
        "http" => "ws",
        _ => return None,
    };
    let host = u.host_str()?;
    let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
    let namespace = match u.path() {
        "" | "/" => "/".to_string(),
        p => p.trim_end_matches('/').to_string(),
    };
    let query = u.query().map(|q| format!("&{q}")).unwrap_or_default();
    Some((format!("{scheme}://{host}{port}/socket.io/?EIO=4&transport=websocket{query}"), namespace))
}

enum Ended {
    /// Time to renew the endpoint (or the server closed cleanly).
    Renew,
}

impl Engine {
    pub(crate) async fn push_channel_loop(self: Arc<Self>) {
        let mut backoff = Duration::from_secs(1);
        loop {
            if self.cancel.is_cancelled() {
                return;
            }
            if !self.cfg().push_notifications || self.is_auth_required() || self.fatal_error().is_some() {
                if self.sleep_or_cancel(Duration::from_secs(60)).await {
                    return;
                }
                continue;
            }
            let endpoint = self.graph.socket_io_endpoint().await;
            let ep = match endpoint {
                Ok(ep) => ep,
                Err(e) => {
                    let wait = if e.is_not_found() || e.status() == Some(400) {
                        Duration::from_secs(1800) // not offered for this drive
                    } else {
                        backoff
                    };
                    tracing::debug!("push channel unavailable: {e}");
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    if self.sleep_or_cancel(wait).await {
                        return;
                    }
                    continue;
                }
            };
            let renew_at = ep
                .expiration_date_time
                .map(|t| (t - chrono::Utc::now()).to_std().unwrap_or_default().saturating_sub(Duration::from_secs(120)))
                .unwrap_or(Duration::from_secs(3600))
                .max(Duration::from_secs(60));
            match self.run_socket(&ep.notification_url, renew_at).await {
                Ok(Ended::Renew) => backoff = Duration::from_secs(1),
                Err(e) => {
                    tracing::info!("push channel closed: {e}");
                    if self.sleep_or_cancel(backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }
            self.push_connected.store(false, Ordering::SeqCst);
        }
    }

    /// Sleep; returns true if the engine was stopped meanwhile.
    async fn sleep_or_cancel(&self, d: Duration) -> bool {
        tokio::select! {
            _ = self.cancel.cancelled() => true,
            _ = tokio::time::sleep(d) => false,
        }
    }

    async fn run_socket(&self, notification_url: &str, renew_after: Duration) -> anyhow::Result<Ended> {
        let (ws_url, namespace) = socket_url(notification_url).ok_or_else(|| anyhow::anyhow!("bad notificationUrl"))?;
        let mut req = ws_url.as_str().into_client_request()?;
        req.headers_mut().insert("User-Agent", od_graph::client::user_agent().parse()?);
        let (mut ws, _) =
            tokio::time::timeout(Duration::from_secs(30), tokio_tungstenite::connect_async(req)).await??;

        // Engine.IO OPEN packet: 0{"sid":…,"pingInterval":…,"pingTimeout":…}
        let open = match tokio::time::timeout(Duration::from_secs(30), ws.next()).await? {
            Some(Ok(Message::Text(t))) if t.starts_with('0') => t,
            other => anyhow::bail!("unexpected opening packet: {other:?}"),
        };
        let cfg: serde_json::Value = serde_json::from_str(&open[1..]).unwrap_or_default();
        let ping_interval = cfg["pingInterval"].as_u64().unwrap_or(25_000);
        let ping_timeout = cfg["pingTimeout"].as_u64().unwrap_or(20_000);
        let dead_after = Duration::from_millis(ping_interval + ping_timeout + 5_000);

        let connect = if namespace == "/" { "40".to_string() } else { format!("40{namespace},") };
        ws.send(Message::Text(connect.into())).await?;
        let event_prefix = if namespace == "/" { "42".to_string() } else { format!("42{namespace},") };

        let deadline = tokio::time::Instant::now() + renew_after;
        let mut pending_since: Option<tokio::time::Instant> = None;
        loop {
            let wait = match pending_since {
                // Debounce bursts of notifications into one delta round.
                Some(t) => (t + Duration::from_secs(2)).saturating_duration_since(tokio::time::Instant::now()),
                None => dead_after,
            };
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    let _ = ws.close(None).await;
                    return Ok(Ended::Renew);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let _ = ws.close(None).await;
                    return Ok(Ended::Renew);
                }
                r = tokio::time::timeout(wait, ws.next()) => match r {
                    Err(_) if pending_since.is_some() => {
                        pending_since = None;
                        tracing::debug!("change notification received; checking for changes");
                        self.pull_wake.notify_one();
                    }
                    Err(_) => anyhow::bail!("no heartbeat from the notification server"),
                    Ok(None) => anyhow::bail!("connection closed"),
                    Ok(Some(Err(e))) => return Err(e.into()),
                    Ok(Some(Ok(Message::Text(t)))) => {
                        let t = t.as_str();
                        if t == "2" {
                            ws.send(Message::Text("3".into())).await?;
                        } else if t.starts_with(&event_prefix) {
                            pending_since.get_or_insert_with(tokio::time::Instant::now);
                        } else if t.starts_with("40") {
                            if !self.push_connected.swap(true, Ordering::SeqCst) {
                                tracing::info!("real-time change notifications connected");
                            }
                        } else if t.starts_with("44") {
                            anyhow::bail!("notification server refused the connection: {t}");
                        } else if t.starts_with("41") || t == "1" {
                            return Ok(Ended::Renew);
                        }
                    }
                    Ok(Some(Ok(Message::Close(_)))) => return Ok(Ended::Renew),
                    Ok(Some(Ok(_))) => {}
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Network test (run with `cargo test -- --ignored`): TLS WebSockets work
    /// with the compiled-in crypto provider.
    #[tokio::test]
    #[ignore]
    async fn tls_websocket_connects() {
        let (mut ws, _) = tokio_tungstenite::connect_async("wss://echo.websocket.org").await.expect("wss connect");
        ws.send(Message::Text("hello".into())).await.unwrap();
        let _ = ws.next().await;
    }

    #[test]
    fn builds_socket_url() {
        let (u, ns) =
            socket_url("https://southcentralus1-7.pushnp.svc.ms/notifications?token=abc&applicationId=xyz").unwrap();
        assert_eq!(
            u,
            "wss://southcentralus1-7.pushnp.svc.ms/socket.io/?EIO=4&transport=websocket&token=abc&applicationId=xyz"
        );
        assert_eq!(ns, "/notifications");
        let (u, _) = socket_url("http://127.0.0.1:8080/notifications?token=t").unwrap();
        assert!(u.starts_with("ws://127.0.0.1:8080/socket.io/?EIO=4"));
    }
}
