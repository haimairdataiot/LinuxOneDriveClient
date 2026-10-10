//! SharePoint sites and document libraries (research doc 01 §5.4).

use crate::client::GraphClient;
use crate::error::Result;
use crate::model::{Drive, ValueList};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Site {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub web_url: Option<String>,
}

impl Site {
    pub fn title(&self) -> String {
        self.display_name.clone().or_else(|| self.name.clone()).unwrap_or_else(|| self.id.clone())
    }
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

impl GraphClient {
    /// Sites the user can find (`*` lists followed and frequent ones).
    pub async fn search_sites(&self, query: &str) -> Result<Vec<Site>> {
        let q = if query.trim().is_empty() { "*" } else { query.trim() };
        let list: ValueList<Site> = self.get_json(&self.url(&format!("/sites?search={}", enc(q)))).await?;
        Ok(list.value)
    }

    pub async fn site(&self, site_id: &str) -> Result<Site> {
        self.get_json(&self.url(&format!("/sites/{site_id}"))).await
    }

    /// Document libraries of a site.
    pub async fn site_drives(&self, site_id: &str) -> Result<Vec<Drive>> {
        let list: ValueList<Drive> = self.get_json(&self.url(&format!("/sites/{site_id}/drives"))).await?;
        Ok(list.value)
    }

    /// The drive behind a SharePoint list, as named by the browser's Sync
    /// button: site `{host},{siteId},{webId}` and list id.
    pub async fn list_drive(&self, site: &str, list_id: &str) -> Result<Drive> {
        let list = list_id.trim_matches(['{', '}']);
        self.get_json(&self.url(&format!("/sites/{site}/lists/{list}/drive"))).await
    }

    pub async fn drive_by_id(&self, drive_id: &str) -> Result<Drive> {
        self.get_json(&self.url(&format!("/drives/{drive_id}"))).await
    }
}
