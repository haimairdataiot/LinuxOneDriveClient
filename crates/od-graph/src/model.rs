//! Typed subsets of Microsoft Graph resources used by the sync engine.
//! Unknown fields are ignored; every optional facet is `Option`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DriveItem {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub size: Option<i64>,
    #[serde(default, rename = "eTag")]
    pub e_tag: Option<String>,
    #[serde(default, rename = "cTag")]
    pub c_tag: Option<String>,
    #[serde(default)]
    pub created_date_time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_modified_date_time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub web_url: Option<String>,
    #[serde(default)]
    pub parent_reference: Option<ItemReference>,
    #[serde(default)]
    pub file: Option<FileFacet>,
    #[serde(default)]
    pub folder: Option<FolderFacet>,
    #[serde(default)]
    pub package: Option<PackageFacet>,
    #[serde(default)]
    pub file_system_info: Option<FileSystemInfo>,
    #[serde(default)]
    pub root: Option<serde_json::Value>,
    #[serde(default)]
    pub deleted: Option<serde_json::Value>,
    #[serde(default)]
    pub remote_item: Option<Box<DriveItem>>,
    #[serde(default)]
    pub shared: Option<serde_json::Value>,
    #[serde(default)]
    pub malware: Option<serde_json::Value>,
    #[serde(default)]
    pub last_modified_by: Option<IdentitySet>,
    #[serde(default, rename = "@microsoft.graph.downloadUrl")]
    pub download_url: Option<String>,
}

impl DriveItem {
    pub fn is_folder(&self) -> bool {
        self.folder.is_some() || self.root.is_some() || self.remote_item.as_ref().is_some_and(|r| r.folder.is_some())
    }
    pub fn is_file(&self) -> bool {
        self.file.is_some() && self.folder.is_none()
    }
    pub fn is_deleted(&self) -> bool {
        self.deleted.is_some()
    }
    pub fn is_root(&self) -> bool {
        self.root.is_some()
    }
    pub fn is_package(&self) -> bool {
        self.package.is_some()
    }
    pub fn quick_xor_hash(&self) -> Option<&str> {
        self.file.as_ref().and_then(|f| f.hashes.as_ref()).and_then(|h| h.quick_xor_hash.as_deref())
    }
    pub fn parent_id(&self) -> Option<&str> {
        self.parent_reference.as_ref().and_then(|p| p.id.as_deref())
    }
    /// Modification time as the user sees it (client-set when available).
    pub fn modified(&self) -> Option<DateTime<Utc>> {
        self.file_system_info.as_ref().and_then(|f| f.last_modified_date_time).or(self.last_modified_date_time)
    }
    pub fn created(&self) -> Option<DateTime<Utc>> {
        self.file_system_info.as_ref().and_then(|f| f.created_date_time).or(self.created_date_time)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ItemReference {
    #[serde(default)]
    pub drive_id: Option<String>,
    #[serde(default)]
    pub drive_type: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileFacet {
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub hashes: Option<Hashes>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Hashes {
    #[serde(default)]
    pub quick_xor_hash: Option<String>,
    #[serde(default)]
    pub sha1_hash: Option<String>,
    #[serde(default)]
    pub sha256_hash: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FolderFacet {
    #[serde(default)]
    pub child_count: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PackageFacet {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_date_time: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified_date_time: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IdentitySet {
    #[serde(default)]
    pub user: Option<Identity>,
    #[serde(default)]
    pub application: Option<Identity>,
}

impl IdentitySet {
    pub fn display_name(&self) -> String {
        self.user.as_ref().or(self.application.as_ref()).and_then(|i| i.display_name.clone()).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Drive {
    pub id: String,
    #[serde(default)]
    pub drive_type: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub web_url: Option<String>,
    #[serde(default)]
    pub owner: Option<IdentitySet>,
    #[serde(default)]
    pub quota: Option<Quota>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Quota {
    #[serde(default)]
    pub total: Option<i64>,
    #[serde(default)]
    pub used: Option<i64>,
    #[serde(default)]
    pub remaining: Option<i64>,
    #[serde(default)]
    pub deleted: Option<i64>,
    /// "normal" | "nearing" | "critical" | "exceeded"
    #[serde(default)]
    pub state: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct User {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub mail: Option<String>,
    #[serde(default)]
    pub user_principal_name: Option<String>,
}

impl User {
    pub fn email(&self) -> String {
        self.mail.clone().filter(|m| !m.is_empty()).or_else(|| self.user_principal_name.clone()).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Organization {
    #[serde(default, rename = "displayName")]
    pub display_name: Option<String>,
}

/// One page of a delta (or children) enumeration.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ItemPage {
    #[serde(default)]
    pub value: Vec<DriveItem>,
    #[serde(default, rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    #[serde(default, rename = "@odata.deltaLink")]
    pub delta_link: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadSession {
    /// Present when the session is created; absent in fragment/status responses.
    #[serde(default)]
    pub upload_url: String,
    #[serde(default)]
    pub expiration_date_time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub next_expected_ranges: Vec<String>,
}

impl UploadSession {
    /// First byte the server expects next ("12345-" or "12345-67890").
    pub fn next_offset(&self) -> Option<u64> {
        self.next_expected_ranges.first().and_then(|r| r.split('-').next()).and_then(|s| s.parse().ok())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveItemVersion {
    pub id: String,
    #[serde(default)]
    pub last_modified_date_time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub size: Option<i64>,
    #[serde(default)]
    pub last_modified_by: Option<IdentitySet>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ValueList<T> {
    #[serde(default = "Vec::new")]
    pub value: Vec<T>,
    #[serde(default, rename = "@odata.nextLink")]
    pub next_link: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ThumbnailSet {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub small: Option<Thumbnail>,
    #[serde(default)]
    pub medium: Option<Thumbnail>,
    #[serde(default)]
    pub large: Option<Thumbnail>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Thumbnail {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

/// Response of `GET …/subscriptions/socketIo`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocketIoEndpoint {
    #[serde(default)]
    pub id: Option<String>,
    pub notification_url: String,
    #[serde(default)]
    pub expiration_date_time: Option<DateTime<Utc>>,
}

/// Graph error envelope: `{"error": {"code": "...", "message": "...", "innerError": {...}}}`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ErrorEnvelope {
    #[serde(default)]
    pub error: Option<ErrorBody>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorBody {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub inner_error: Option<Box<ErrorBody>>,
}

impl ErrorBody {
    /// Innermost (most specific) error code, e.g. `resyncRequired`.
    pub fn innermost_code(&self) -> &str {
        let mut cur = self;
        while let Some(inner) = cur.inner_error.as_deref() {
            if inner.code.is_empty() {
                break;
            }
            cur = inner;
        }
        &cur.code
    }

    pub fn has_code(&self, code: &str) -> bool {
        let mut cur = Some(self);
        while let Some(c) = cur {
            if c.code.eq_ignore_ascii_case(code) {
                return true;
            }
            cur = c.inner_error.as_deref();
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_delta_item() {
        let json = r#"{
            "id": "01ABC", "name": "report.docx", "size": 1234, "eTag": "\"{X},2\"", "cTag": "\"c:{X},3\"",
            "lastModifiedDateTime": "2025-02-03T04:05:06Z",
            "parentReference": {"driveId": "d1", "driveType": "personal", "id": "01PARENT"},
            "file": {"mimeType": "application/x", "hashes": {"quickXorHash": "AAAA"}},
            "fileSystemInfo": {"lastModifiedDateTime": "2025-01-01T00:00:00Z"},
            "shared": {"scope": "users"},
            "@microsoft.graph.downloadUrl": "https://x/y"
        }"#;
        let item: DriveItem = serde_json::from_str(json).unwrap();
        assert!(item.is_file() && !item.is_folder());
        assert_eq!(item.quick_xor_hash(), Some("AAAA"));
        assert_eq!(item.parent_id(), Some("01PARENT"));
        assert_eq!(item.modified().unwrap().to_rfc3339(), "2025-01-01T00:00:00+00:00");
        assert_eq!(item.download_url.as_deref(), Some("https://x/y"));
        let del: DriveItem = serde_json::from_str(r#"{"id":"Z","deleted":{"state":"deleted"}}"#).unwrap();
        assert!(del.is_deleted());
    }

    #[test]
    fn error_codes() {
        let e: ErrorEnvelope = serde_json::from_str(
            r#"{"error":{"code":"resyncRequired","message":"m","innerError":{"code":"resyncChangesApplyDifferences"}}}"#,
        )
        .unwrap();
        let b = e.error.unwrap();
        assert!(b.has_code("resyncrequired"));
        assert_eq!(b.innermost_code(), "resyncChangesApplyDifferences");
    }
}
