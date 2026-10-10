//! Microsoft identity platform + Microsoft Graph (OneDrive) client.

pub mod auth;
pub mod client;
pub mod error;
pub mod model;
pub use od_core::quickxor;
pub mod ratelimit;
pub mod sharing;
pub mod sites;

pub use auth::{OAuthConfig, TokenManager, TokenSet};
pub use client::{ConflictBehavior, FragmentResult, GraphClient, ItemPatch, UploadTarget};
pub use error::{GraphError, Result};
pub use model::DriveItem;
