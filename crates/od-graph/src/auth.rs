//! Microsoft identity platform (v2.0 endpoints) for a native public client:
//!
//! * authorization code flow with PKCE and a loopback redirect
//!   (`http://localhost:<port>`), opened in the user's browser;
//! * device code flow for headless systems;
//! * refresh-token rotation, with a persistence hook so every new refresh
//!   token is saved before it is used.

use crate::error::{GraphError, Result};
use base64::Engine as _;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

pub const DEFAULT_LOGIN_BASE: &str = "https://login.microsoftonline.com";
/// Delegated scopes requested at sign-in.
pub const DEFAULT_SCOPES: &str = "openid profile email offline_access User.Read Files.ReadWrite.All";
/// Tenant id that identifies personal Microsoft accounts in id tokens.
pub const MSA_TENANT_ID: &str = "9188040d-6c67-4c5b-b112-36a304b66dad";

#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub client_id: String,
    pub login_base: String,
    /// "common", "consumers", "organizations" or a tenant id.
    pub tenant: String,
    pub scopes: String,
}

impl OAuthConfig {
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            login_base: std::env::var("LINUX_ONEDRIVE_LOGIN_URL").unwrap_or_else(|_| DEFAULT_LOGIN_BASE.to_string()),
            tenant: "common".into(),
            scopes: DEFAULT_SCOPES.into(),
        }
    }

    fn endpoint(&self, name: &str) -> String {
        format!("{}/{}/oauth2/v2.0/{}", self.login_base.trim_end_matches('/'), self.tenant, name)
    }
}

/// Claims we read (without verification; display purposes only) from the id token.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IdClaims {
    #[serde(default)]
    pub tid: Option<String>,
    #[serde(default)]
    pub oid: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub preferred_username: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

impl IdClaims {
    pub fn is_personal(&self) -> bool {
        self.tid.as_deref() == Some(MSA_TENANT_ID)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub claims: Option<IdClaims>,
}

impl TokenSet {
    pub fn expires_soon(&self) -> bool {
        self.expires_at - Utc::now() < ChronoDuration::minutes(5)
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct TokenErrorResponse {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
}

/// PKCE verifier/challenge pair (S256).
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Self {
        let verifier = random_urlsafe(64);
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        Self { verifier, challenge }
    }
}

/// Random URL-safe string of `n` characters.
pub fn random_urlsafe(n: usize) -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::rng();
    (0..n).map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char).collect()
}

/// Redirect URI for the embedded web view flow (registered for native clients).
pub fn native_client_redirect(cfg: &OAuthConfig) -> String {
    format!("{}/common/oauth2/nativeclient", cfg.login_base.trim_end_matches('/'))
}

/// Personal Microsoft account domains: skip the account-type picker for them.
const CONSUMER_DOMAINS: &[&str] = &["outlook.com", "hotmail.com", "live.com", "msn.com", "passport.com"];

/// `domain_hint` for a login hint, if the account type is obvious from the address.
pub fn domain_hint(login_hint: &str) -> Option<&'static str> {
    let domain = login_hint.rsplit_once('@')?.1.to_ascii_lowercase();
    let is_consumer = CONSUMER_DOMAINS
        .iter()
        .any(|d| domain == *d || domain.starts_with(&format!("{}.", d.trim_end_matches(".com"))));
    is_consumer.then_some("consumers")
}

/// Extract the authorization code from the redirect URL the web view (or
/// loopback listener) received, validating `state`.
pub fn code_from_redirect(redirect_url: &str, expected_state: &str) -> Result<String> {
    let url = url::Url::parse(redirect_url).map_err(|e| GraphError::InvalidResponse(e.to_string()))?;
    let mut params: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    if let Some(frag) = url.fragment() {
        params.extend(url::form_urlencoded::parse(frag.as_bytes()).into_owned());
    }
    if params.get("state").map(String::as_str) != Some(expected_state) {
        return Err(GraphError::AuthRequired("state mismatch in sign-in response".into()));
    }
    if let Some(err) = params.get("error") {
        let desc = params.get("error_description").cloned().unwrap_or_default();
        return Err(GraphError::AuthRequired(format!("{err}: {}", first_line(&desc))));
    }
    params.remove("code").ok_or_else(|| GraphError::InvalidResponse("no code in redirect".into()))
}

/// Build the URL for the authorization request.
pub fn authorize_url(
    cfg: &OAuthConfig,
    redirect_uri: &str,
    state: &str,
    pkce: &Pkce,
    login_hint: Option<&str>,
) -> String {
    let mut url = url::Url::parse(&cfg.endpoint("authorize")).expect("valid login base");
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", &cfg.client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("response_mode", "query")
            .append_pair("scope", &cfg.scopes)
            .append_pair("state", state)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256");
        match login_hint.filter(|h| !h.is_empty()) {
            Some(h) => {
                q.append_pair("login_hint", h);
                if let Some(d) = domain_hint(h) {
                    q.append_pair("domain_hint", d);
                }
            }
            None => {
                q.append_pair("prompt", "select_account");
            }
        }
    }
    url.into()
}

/// A one-shot HTTP listener that captures the redirect. It listens on
/// 127.0.0.1 and, when available, on [::1] with the same port, because
/// browsers may resolve `localhost` to either address.
pub struct LoopbackServer {
    listener: TcpListener,
    listener6: Option<TcpListener>,
    port: u16,
}

impl LoopbackServer {
    pub async fn bind() -> Result<Self> {
        // Retry a few times in case the IPv6 side of a random port is taken.
        let mut last_err = None;
        for _ in 0..8 {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
            let port = listener.local_addr()?.port();
            match TcpListener::bind(("::1", port)).await {
                Ok(l6) => return Ok(Self { listener, listener6: Some(l6), port }),
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => last_err = Some(e),
                // No IPv6 loopback on this machine: IPv4 alone is fine.
                Err(_) => return Ok(Self { listener, listener6: None, port }),
            }
        }
        Err(last_err.map(GraphError::Io).unwrap_or_else(|| GraphError::InvalidResponse("bind".into())))
    }

    async fn accept(&self) -> std::io::Result<tokio::net::TcpStream> {
        match &self.listener6 {
            Some(l6) => tokio::select! {
                r = self.listener.accept() => r.map(|(s, _)| s),
                r = l6.accept() => r.map(|(s, _)| s),
            },
            None => self.listener.accept().await.map(|(s, _)| s),
        }
    }

    /// Redirect URI to register with the request. Entra ID ignores the port
    /// of `http://localhost` redirect URIs registered for public clients.
    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{}", self.port)
    }

    /// Wait for the browser to deliver `?code=…&state=…`. Returns the code.
    pub async fn wait_for_code(self, expected_state: &str, timeout: Duration) -> Result<String> {
        let fut = async {
            loop {
                let mut sock = self.accept().await?;
                let mut buf = vec![0u8; 16 * 1024];
                let mut len = 0;
                // Read until the end of the request head.
                loop {
                    let n = sock.read(&mut buf[len..]).await?;
                    if n == 0 {
                        break;
                    }
                    len += n;
                    if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") || len == buf.len() {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&buf[..len]).to_string();
                let target = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/").to_string();
                let url = url::Url::parse(&format!("http://localhost{target}"))
                    .map_err(|e| GraphError::InvalidResponse(e.to_string()))?;
                let params: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();

                if params.is_empty() {
                    // favicon or a stray request: answer and keep waiting.
                    let _ = sock
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    continue;
                }

                let outcome = if params.get("state").map(String::as_str) != Some(expected_state) {
                    Err(GraphError::AuthRequired("state mismatch in sign-in response".into()))
                } else if let Some(err) = params.get("error") {
                    let desc = params.get("error_description").cloned().unwrap_or_default();
                    Err(GraphError::AuthRequired(format!("{err}: {desc}")))
                } else if let Some(code) = params.get("code") {
                    Ok(code.clone())
                } else {
                    Err(GraphError::InvalidResponse("no code in redirect".into()))
                };
                let body = match &outcome {
                    Ok(_) => SUCCESS_PAGE.to_string(),
                    Err(e) => FAILURE_PAGE.replace("{error}", &html_escape(&e.to_string())),
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
                return outcome;
            }
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(r) => r,
            Err(_) => Err(GraphError::AuthRequired("sign-in timed out".into())),
        }
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

const SUCCESS_PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Signed in</title>
<style>body{font-family:Cantarell,system-ui,sans-serif;display:flex;align-items:center;justify-content:center;height:100vh;margin:0;background:#fafafa;color:#2e3436}
.c{text-align:center}h1{font-weight:600;font-size:1.6em}p{opacity:.75}</style></head>
<body><div class="c"><h1>You're signed in</h1><p>You can close this tab and return to Linux OneDrive.</p></div></body></html>"#;

const FAILURE_PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Sign-in failed</title>
<style>body{font-family:Cantarell,system-ui,sans-serif;display:flex;align-items:center;justify-content:center;height:100vh;margin:0;background:#fafafa;color:#2e3436}
.c{text-align:center;max-width:36em}h1{font-weight:600;font-size:1.6em}code{opacity:.75}</style></head>
<body><div class="c"><h1>Sign-in didn't complete</h1><p><code>{error}</code></p><p>Return to Linux OneDrive and try again.</p></div></body></html>"#;

async fn token_request(http: &reqwest::Client, cfg: &OAuthConfig, form: &[(&str, &str)]) -> Result<TokenSet> {
    let resp = http.post(cfg.endpoint("token")).form(form).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        let err: TokenErrorResponse = serde_json::from_str(&text).unwrap_or_default();
        return Err(match err.error.as_str() {
            "invalid_grant"
            | "interaction_required"
            | "consent_required"
            | "login_required"
            | "invalid_client"
            | "unauthorized_client" => {
                GraphError::AuthRequired(format!("{}: {}", err.error, first_line(&err.error_description)))
            }
            _ => GraphError::Api {
                status: status.as_u16(),
                code: err.error,
                inner_code: String::new(),
                message: first_line(&err.error_description).to_string(),
                retry_after: None,
            },
        });
    }
    let tr: TokenResponse =
        serde_json::from_str(&text).map_err(|e| GraphError::InvalidResponse(format!("token response: {e}")))?;
    let claims = tr.id_token.as_deref().and_then(decode_id_token);
    Ok(TokenSet {
        access_token: tr.access_token,
        refresh_token: tr.refresh_token.unwrap_or_default(),
        expires_at: Utc::now() + ChronoDuration::seconds(tr.expires_in.unwrap_or(3600).max(60)),
        scope: tr.scope.unwrap_or_default(),
        claims,
    })
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// Decode (without verifying) the payload of a JWT id token.
pub fn decode_id_token(jwt: &str) -> Option<IdClaims> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Exchange an authorization code for tokens.
pub async fn exchange_code(
    http: &reqwest::Client,
    cfg: &OAuthConfig,
    code: &str,
    redirect_uri: &str,
    pkce: &Pkce,
) -> Result<TokenSet> {
    token_request(
        http,
        cfg,
        &[
            ("client_id", cfg.client_id.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", pkce.verifier.as_str()),
            ("scope", cfg.scopes.as_str()),
        ],
    )
    .await
}

/// Redeem a refresh token. Microsoft rotates refresh tokens: always persist the result.
pub async fn refresh(http: &reqwest::Client, cfg: &OAuthConfig, refresh_token: &str) -> Result<TokenSet> {
    let mut ts = token_request(
        http,
        cfg,
        &[
            ("client_id", cfg.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", cfg.scopes.as_str()),
        ],
    )
    .await?;
    if ts.refresh_token.is_empty() {
        ts.refresh_token = refresh_token.to_string();
    }
    Ok(ts)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    #[serde(default = "default_interval")]
    pub interval: u64,
    #[serde(default)]
    pub message: String,
}

fn default_interval() -> u64 {
    5
}

/// Start the device code flow.
pub async fn start_device_flow(http: &reqwest::Client, cfg: &OAuthConfig) -> Result<DeviceCode> {
    let resp = http
        .post(cfg.endpoint("devicecode"))
        .form(&[("client_id", cfg.client_id.as_str()), ("scope", cfg.scopes.as_str())])
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        let err: TokenErrorResponse = serde_json::from_str(&text).unwrap_or_default();
        return Err(GraphError::AuthRequired(format!("{}: {}", err.error, first_line(&err.error_description))));
    }
    serde_json::from_str(&text).map_err(|e| GraphError::InvalidResponse(format!("device code: {e}")))
}

/// Poll until the user completes the device code sign-in (or it expires).
pub async fn poll_device_flow(http: &reqwest::Client, cfg: &OAuthConfig, dc: &DeviceCode) -> Result<TokenSet> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(dc.expires_in.max(60));
    let mut interval = Duration::from_secs(dc.interval.max(1));
    loop {
        tokio::time::sleep(interval).await;
        if tokio::time::Instant::now() > deadline {
            return Err(GraphError::AuthRequired("the sign-in code expired".into()));
        }
        let resp = http
            .post(cfg.endpoint("token"))
            .form(&[
                ("client_id", cfg.client_id.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", dc.device_code.as_str()),
            ])
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if status.is_success() {
            let tr: TokenResponse =
                serde_json::from_str(&text).map_err(|e| GraphError::InvalidResponse(format!("token response: {e}")))?;
            let claims = tr.id_token.as_deref().and_then(decode_id_token);
            return Ok(TokenSet {
                access_token: tr.access_token,
                refresh_token: tr.refresh_token.unwrap_or_default(),
                expires_at: Utc::now() + ChronoDuration::seconds(tr.expires_in.unwrap_or(3600)),
                scope: tr.scope.unwrap_or_default(),
                claims,
            });
        }
        let err: TokenErrorResponse = serde_json::from_str(&text).unwrap_or_default();
        match err.error.as_str() {
            "authorization_pending" => continue,
            "slow_down" => interval += Duration::from_secs(5),
            "authorization_declined" => return Err(GraphError::AuthRequired("sign-in was declined".into())),
            "expired_token" | "code_expired" => {
                return Err(GraphError::AuthRequired("the sign-in code expired".into()));
            }
            other => {
                return Err(GraphError::AuthRequired(format!("{other}: {}", first_line(&err.error_description))));
            }
        }
    }
}

/// Callback invoked with every new token set (so rotated refresh tokens are saved).
pub type PersistFn = Arc<dyn Fn(&TokenSet) + Send + Sync>;

/// Supplies valid access tokens to the Graph client, refreshing as needed.
pub struct TokenManager {
    http: reqwest::Client,
    cfg: std::sync::RwLock<OAuthConfig>,
    tokens: Mutex<TokenSet>,
    persist: PersistFn,
}

impl TokenManager {
    pub fn new(http: reqwest::Client, cfg: OAuthConfig, tokens: TokenSet, persist: PersistFn) -> Self {
        Self { http, cfg: std::sync::RwLock::new(cfg), tokens: Mutex::new(tokens), persist }
    }

    /// A token valid for at least five more minutes.
    pub async fn access_token(&self) -> Result<String> {
        let mut t = self.tokens.lock().await;
        if t.expires_soon() {
            self.refresh_locked(&mut t).await?;
        }
        Ok(t.access_token.clone())
    }

    /// Refresh even if the current token looks valid (after a 401).
    pub async fn force_refresh(&self, rejected: &str) -> Result<String> {
        let mut t = self.tokens.lock().await;
        if t.access_token == rejected {
            self.refresh_locked(&mut t).await?;
        }
        Ok(t.access_token.clone())
    }

    async fn refresh_locked(&self, t: &mut TokenSet) -> Result<()> {
        if t.refresh_token.is_empty() {
            return Err(GraphError::AuthRequired("no refresh token".into()));
        }
        let cfg = self.cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
        let mut new = refresh(&self.http, &cfg, &t.refresh_token).await?;
        if new.claims.is_none() {
            new.claims = t.claims.clone();
        }
        (self.persist)(&new);
        *t = new;
        Ok(())
    }

    /// Scopes used when refreshing (after granting extra ones incrementally).
    pub fn set_scopes(&self, scopes: &str) {
        self.cfg.write().unwrap_or_else(|e| e.into_inner()).scopes = scopes.to_string();
    }

    /// Replace the tokens after an interactive re-authentication.
    pub async fn replace(&self, tokens: TokenSet) {
        (self.persist)(&tokens);
        *self.tokens.lock().await = tokens;
    }

    pub async fn claims(&self) -> Option<IdClaims> {
        self.tokens.lock().await.claims.clone()
    }

    /// Static token for tests and the mock server.
    pub fn fixed(http: reqwest::Client, access_token: &str) -> Self {
        Self::new(
            http,
            OAuthConfig::new("test"),
            TokenSet {
                access_token: access_token.into(),
                refresh_token: String::new(),
                expires_at: Utc::now() + ChronoDuration::days(3650),
                scope: String::new(),
                claims: None,
            },
            Arc::new(|_| {}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_and_url() {
        let p = Pkce::generate();
        assert_eq!(p.verifier.len(), 64);
        assert!(!p.challenge.contains('='));
        let cfg = OAuthConfig {
            client_id: "cid".into(),
            login_base: DEFAULT_LOGIN_BASE.into(),
            tenant: "common".into(),
            scopes: DEFAULT_SCOPES.into(),
        };
        let u = authorize_url(&cfg, "http://localhost:1234", "st", &p, None);
        assert!(u.starts_with("https://login.microsoftonline.com/common/oauth2/v2.0/authorize?"));
        assert!(u.contains("code_challenge_method=S256"));
        assert!(u.contains("prompt=select_account"));
        assert!(u.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1234"));
    }

    #[test]
    fn hints_and_redirect_parsing() {
        assert_eq!(domain_hint("ann@outlook.com"), Some("consumers"));
        assert_eq!(domain_hint("ann@Hotmail.co.uk"), Some("consumers"));
        assert_eq!(domain_hint("ann@contoso.com"), None);
        assert_eq!(domain_hint("not-an-email"), None);
        let cfg = OAuthConfig::new("cid");
        assert!(native_client_redirect(&cfg).ends_with("/common/oauth2/nativeclient"));
        let p = Pkce::generate();
        let u = authorize_url(&cfg, "http://x", "s", &p, Some("bob@live.com"));
        assert!(u.contains("login_hint=bob%40live.com") && u.contains("domain_hint=consumers"));
        assert!(!u.contains("prompt="));

        let ok = "https://login.microsoftonline.com/common/oauth2/nativeclient?code=ABC&state=s1";
        assert_eq!(code_from_redirect(ok, "s1").unwrap(), "ABC");
        assert!(code_from_redirect(ok, "other").is_err());
        let denied = "https://login.microsoftonline.com/common/oauth2/nativeclient?error=access_denied&error_description=User+cancelled&state=s1";
        let e = code_from_redirect(denied, "s1").unwrap_err();
        assert!(e.to_string().contains("access_denied"));
    }

    #[test]
    fn id_token_claims() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            br#"{"tid":"9188040d-6c67-4c5b-b112-36a304b66dad","name":"Ann","preferred_username":"ann@outlook.com"}"#,
        );
        let jwt = format!("eyJhbGciOiJub25lIn0.{payload}.sig");
        let c = decode_id_token(&jwt).unwrap();
        assert!(c.is_personal());
        assert_eq!(c.preferred_username.as_deref(), Some("ann@outlook.com"));
    }

    #[tokio::test]
    async fn loopback_captures_code() {
        let server = LoopbackServer::bind().await.unwrap();
        let uri = server.redirect_uri();
        let port: u16 = uri.rsplit(':').next().unwrap().parse().unwrap();
        let task = tokio::spawn(server.wait_for_code("xyz", Duration::from_secs(5)));
        let http = reqwest::Client::new();
        let _ = http.get(format!("http://127.0.0.1:{port}/favicon.ico")).send().await;
        let body = http
            .get(format!("http://127.0.0.1:{port}/?code=THECODE&state=xyz"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("signed in"));
        assert_eq!(task.await.unwrap().unwrap(), "THECODE");
    }
}
