use std::time::Duration;

/// Errors from the identity platform or Microsoft Graph.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    /// The refresh token is invalid/expired/revoked: the user must sign in again.
    #[error("sign-in required: {0}")]
    AuthRequired(String),

    /// Graph returned an error response.
    #[error("HTTP {status} {code}: {message}")]
    Api { status: u16, code: String, inner_code: String, message: String, retry_after: Option<Duration> },

    /// The delta token is no longer valid; a full resync is needed. Carries the
    /// restart link from the 410 `Location` header when the server sent one.
    #[error("resync required ({code})")]
    ResyncRequired { code: String, restart_link: Option<String> },

    #[error("downloaded content does not match the expected hash")]
    HashMismatch,

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid response: {0}")]
    InvalidResponse(String),

    #[error("operation cancelled")]
    Cancelled,
}

pub type Result<T, E = GraphError> = std::result::Result<T, E>;

impl GraphError {
    pub fn status(&self) -> Option<u16> {
        match self {
            GraphError::Api { status, .. } => Some(*status),
            GraphError::Network(e) => e.status().map(|s| s.as_u16()),
            _ => None,
        }
    }

    /// Worth retrying automatically (with backoff).
    pub fn is_transient(&self) -> bool {
        match self {
            GraphError::Network(e) => {
                e.is_timeout() || e.is_connect() || e.is_request() || e.is_body() || e.is_decode()
            }
            GraphError::Api { status, .. } => {
                matches!(*status, 408 | 409 | 423 | 429 | 500 | 502 | 503 | 504 | 509)
                    && !self.has_code("nameAlreadyExists")
            }
            GraphError::HashMismatch => true,
            GraphError::Io(_) => false,
            _ => false,
        }
    }

    /// The network seems to be unavailable (as opposed to a server error).
    pub fn is_offline(&self) -> bool {
        matches!(self, GraphError::Network(e) if e.is_connect() || e.is_timeout())
    }

    pub fn is_not_found(&self) -> bool {
        self.status() == Some(404) || self.has_code("itemNotFound")
    }

    pub fn is_precondition_failed(&self) -> bool {
        self.status() == Some(412) || self.has_code("resourceModified")
    }

    /// An item with the same name already exists in the target folder.
    /// (A 409 without this code is a transient lock conflict and is retried.)
    pub fn is_name_conflict(&self) -> bool {
        self.has_code("nameAlreadyExists")
    }

    pub fn is_quota_exceeded(&self) -> bool {
        self.status() == Some(507) || self.has_code("quotaLimitReached")
    }

    pub fn is_access_denied(&self) -> bool {
        self.status() == Some(403) || self.has_code("accessDenied")
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            GraphError::Api { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    pub fn has_code(&self, code: &str) -> bool {
        match self {
            GraphError::Api { code: c, inner_code, .. } => {
                c.eq_ignore_ascii_case(code) || inner_code.eq_ignore_ascii_case(code)
            }
            _ => false,
        }
    }

    /// Short machine code for persisting as an item error.
    pub fn short_code(&self) -> String {
        match self {
            GraphError::Network(_) => "network".into(),
            GraphError::AuthRequired(_) => "auth-required".into(),
            GraphError::Api { status, code, .. } => {
                if code.is_empty() {
                    format!("http-{status}")
                } else {
                    code.clone()
                }
            }
            GraphError::ResyncRequired { .. } => "resync-required".into(),
            GraphError::HashMismatch => "hash-mismatch".into(),
            GraphError::Io(_) => "io".into(),
            GraphError::InvalidResponse(_) => "invalid-response".into(),
            GraphError::Cancelled => "cancelled".into(),
        }
    }

    /// A sentence suitable for the Sync issues list.
    pub fn user_message(&self) -> String {
        if self.is_quota_exceeded() {
            return "Your OneDrive is full. Free up space online or get more storage.".into();
        }
        if self.is_access_denied() {
            return "You don't have permission to change this item.".into();
        }
        match self {
            GraphError::Network(_) => "Couldn't reach OneDrive. Check your internet connection.".into(),
            GraphError::AuthRequired(_) => "Sign in again to keep syncing.".into(),
            GraphError::Api { message, .. } if !message.is_empty() => message.clone(),
            GraphError::HashMismatch => "The file was damaged during transfer. It will be retried.".into(),
            other => other.to_string(),
        }
    }
}
