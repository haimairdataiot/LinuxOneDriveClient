//! Sharing: links, invitations and "Manage access" (driveItem permissions).
//! Research: docs/research/05-sharing.md.
//!
//! Pitfalls encoded here:
//! * never send `retainInheritedPermissions` (false would wipe permissions);
//! * a link's permission comes from `link.type`, not `roles`;
//! * `0001-01-01T00:00:00Z` means "no expiry";
//! * personal drives fill `grantedTo*`, business the `*V2` fields;
//! * createLink reuses an existing link of the same type and scope for our
//!   app (200) and may keep its old expiry/password: callers check that.

use crate::client::{GraphClient, parse_json};
use crate::error::{GraphError, Result};
use crate::model::IdentitySet;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Permission {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub link: Option<SharingLink>,
    #[serde(default)]
    pub granted_to: Option<IdentitySet>,
    #[serde(default, rename = "grantedToV2")]
    pub granted_to_v2: Option<IdentitySet>,
    #[serde(default)]
    pub granted_to_identities: Vec<IdentitySet>,
    #[serde(default, rename = "grantedToIdentitiesV2")]
    pub granted_to_identities_v2: Vec<IdentitySet>,
    #[serde(default)]
    pub invitation: Option<Invitation>,
    #[serde(default)]
    pub inherited_from: Option<crate::model::ItemReference>,
    #[serde(default)]
    pub expiration_date_time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub has_password: Option<bool>,
    #[serde(default)]
    pub share_id: Option<String>,
    /// Present on failed entries of a 207 invite response.
    #[serde(default)]
    pub error: Option<InviteError>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharingLink {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub web_url: Option<String>,
    #[serde(default)]
    pub prevents_download: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Invitation {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub sign_in_required: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteError {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub localized_message: Option<String>,
    #[serde(default)]
    pub fix_it_url: Option<String>,
}

impl Permission {
    /// Expiry, ignoring the "no expiry" sentinel.
    pub fn expires(&self) -> Option<DateTime<Utc>> {
        self.expiration_date_time.filter(|t| t.timestamp() > 0)
    }

    /// People this permission is for (both personal and business shapes).
    pub fn people(&self) -> Vec<(String, String)> {
        let mut sets: Vec<&IdentitySet> = Vec::new();
        if !self.granted_to_identities_v2.is_empty() {
            sets.extend(self.granted_to_identities_v2.iter());
        } else if !self.granted_to_identities.is_empty() {
            sets.extend(self.granted_to_identities.iter());
        } else if let Some(g) = self.granted_to_v2.as_ref().or(self.granted_to.as_ref()) {
            sets.push(g);
        }
        let mut out: Vec<(String, String)> = sets
            .into_iter()
            .filter_map(|s| s.user.as_ref())
            .map(|u| (u.display_name.clone().unwrap_or_default(), u.email.clone().unwrap_or_default()))
            .collect();
        if let Some(email) = self.invitation.as_ref().and_then(|i| i.email.clone()) {
            if out.is_empty() {
                out.push((String::new(), email));
            } else if let Some(first) = out.first_mut()
                && first.1.is_empty()
            {
                first.1 = email;
            }
        }
        out
    }
}

/// What to create in `create_link_with`.
#[derive(Debug, Clone, Default)]
pub struct LinkRequest {
    /// "view" | "edit" | "embed" | (business, beta) "review" | "blocksDownload"
    pub kind: String,
    /// "anonymous" | "organization" | "users"
    pub scope: String,
    pub expiration: Option<DateTime<Utc>>,
    pub password: Option<String>,
    /// For scope "users" (business, beta endpoint).
    pub recipients: Vec<String>,
    pub send_notification: bool,
}

impl LinkRequest {
    fn needs_beta(&self) -> bool {
        matches!(self.kind.as_str(), "review" | "blocksDownload") || !self.recipients.is_empty()
    }
}

/// Outcome of an invitation (207 partial failures are split out).
#[derive(Debug, Clone, Default)]
pub struct InviteResult {
    pub granted: Vec<Permission>,
    pub failed: Vec<(String, InviteError)>,
}

/// A suggestion for the "Add a name, group or email" field.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PersonHit {
    pub name: String,
    pub email: String,
}

impl GraphClient {
    /// Every permission on an item (owner, links, people, inherited).
    pub async fn list_permissions(&self, item_id: &str) -> Result<Vec<Permission>> {
        let url = self.drive_url(&format!("/items/{item_id}/permissions"));
        let resp = self.execute(|c| c.get(&url)).await?;
        let list: crate::model::ValueList<Permission> = parse_json(resp).await?;
        Ok(list.value)
    }

    /// Create (or get our existing) sharing link. Returns the permission and
    /// whether it was newly created (201) rather than reused (200).
    pub async fn create_link_with(&self, item_id: &str, req: &LinkRequest) -> Result<(Permission, bool)> {
        let path = format!("/items/{item_id}/createLink");
        let url = if req.needs_beta() { self.beta_drive_url(&path) } else { self.drive_url(&path) };
        let mut body = json!({ "type": req.kind, "scope": req.scope });
        if let Some(t) = req.expiration {
            body["expirationDateTime"] = json!(t.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        }
        if let Some(p) = req.password.as_ref().filter(|p| !p.is_empty()) {
            body["password"] = json!(p);
        }
        if !req.recipients.is_empty() {
            body["recipients"] = json!(req.recipients.iter().map(|e| json!({ "email": e })).collect::<Vec<_>>());
            body["sendNotification"] = json!(req.send_notification);
        }
        let resp = self.execute(|c| c.post(&url).json(&body)).await?;
        let created = resp.status() == reqwest::StatusCode::CREATED;
        Ok((parse_json(resp).await?, created))
    }

    /// Share with people. `role`: "read" | "write".
    #[allow(clippy::too_many_arguments)]
    pub async fn invite(
        &self,
        item_id: &str,
        recipients: &[String],
        role: &str,
        message: &str,
        send_invitation: bool,
        expiration: Option<DateTime<Utc>>,
        password: Option<&str>,
    ) -> Result<InviteResult> {
        let url = self.drive_url(&format!("/items/{item_id}/invite"));
        let mut body = json!({
            "recipients": recipients.iter().map(|e| json!({ "email": e })).collect::<Vec<_>>(),
            "roles": [role],
            // Both false is rejected by the service; sign-in is always required.
            "requireSignIn": true,
            "sendInvitation": send_invitation,
        });
        if !message.is_empty() {
            body["message"] = json!(message.chars().take(2000).collect::<String>());
        }
        if let Some(t) = expiration {
            body["expirationDateTime"] = json!(t.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        }
        if let Some(p) = password.filter(|p| !p.is_empty()) {
            body["password"] = json!(p);
        }
        let resp = self.execute(|c| c.post(&url).json(&body)).await?;
        let list: crate::model::ValueList<Permission> = parse_json(resp).await?;
        let mut out = InviteResult::default();
        for (i, p) in list.value.into_iter().enumerate() {
            match p.error.clone() {
                Some(e) => {
                    let email = p
                        .invitation
                        .as_ref()
                        .and_then(|inv| inv.email.clone())
                        .or_else(|| recipients.get(i).cloned())
                        .unwrap_or_default();
                    out.failed.push((email, e));
                }
                None => out.granted.push(p),
            }
        }
        Ok(out)
    }

    /// Change a direct grant's role ("read" | "write").
    pub async fn update_permission_roles(&self, item_id: &str, perm_id: &str, role: &str) -> Result<Permission> {
        let url = self.drive_url(&format!("/items/{item_id}/permissions/{perm_id}"));
        let body = json!({ "roles": [role] });
        let resp = self.execute(|c| c.patch(&url).json(&body)).await?;
        parse_json(resp).await
    }

    /// Remove a link or stop sharing with someone (non-inherited, non-owner only).
    pub async fn delete_permission(&self, item_id: &str, perm_id: &str) -> Result<()> {
        let url = self.drive_url(&format!("/items/{item_id}/permissions/{perm_id}"));
        match self.execute(|c| c.delete(&url)).await {
            Ok(_) => Ok(()),
            Err(e) if e.is_not_found() => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Remove people from a "specific people" link (business only).
    pub async fn revoke_grants(&self, item_id: &str, perm_id: &str, emails: &[String]) -> Result<Permission> {
        let url = self.drive_url(&format!("/items/{item_id}/permissions/{perm_id}/revokeGrants"));
        let body = json!({ "grantees": emails.iter().map(|e| json!({ "email": e })).collect::<Vec<_>>() });
        let resp = self.execute(|c| c.post(&url).json(&body)).await?;
        parse_json(resp).await
    }

    /// Suggestions for the recipient field (needs the People.Read scope).
    pub async fn search_people(&self, query: &str) -> Result<Vec<PersonHit>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Person {
            #[serde(default)]
            display_name: Option<String>,
            #[serde(default)]
            scored_email_addresses: Vec<Scored>,
            #[serde(default)]
            user_principal_name: Option<String>,
        }
        #[derive(Deserialize)]
        struct Scored {
            #[serde(default)]
            address: Option<String>,
        }
        let mut url =
            url::Url::parse(&self.url("/me/people")).map_err(|e| GraphError::InvalidResponse(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("$search", &format!("\"{}\"", query.replace('"', "")))
            .append_pair("$top", "8")
            .append_pair("$select", "displayName,scoredEmailAddresses,userPrincipalName");
        let resp = self.execute(|c| c.get(url.as_str())).await?;
        let list: crate::model::ValueList<Person> = parse_json(resp).await?;
        Ok(list
            .value
            .into_iter()
            .filter_map(|p| {
                let email = p.scored_email_addresses.into_iter().find_map(|s| s.address).or(p.user_principal_name)?;
                Some(PersonHit { name: p.display_name.unwrap_or_default(), email })
            })
            .collect())
    }

    /// Fetch an item's `shared` facet and web URL after a sharing change.
    pub async fn sharing_state(&self, item_id: &str) -> Result<(bool, Option<String>)> {
        let url = self.drive_url(&format!("/items/{item_id}?$select=id,shared,webUrl"));
        let resp = self.execute(|c| c.get(&url)).await?;
        let it: crate::model::DriveItem = parse_json(resp).await?;
        Ok((it.shared.is_some(), it.web_url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn people_from_personal_and_business_shapes() {
        let personal: Permission = serde_json::from_str(
            r#"{"id":"1","roles":["read"],"grantedTo":{"user":{"id":"x","displayName":"Robin"}},
                "invitation":{"email":"robin@contoso.com"},"link":{"webUrl":"https://1drv.ms/t/s!1"},
                "expirationDateTime":"0001-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(personal.people(), vec![("Robin".to_string(), "robin@contoso.com".to_string())]);
        assert!(personal.expires().is_none());
        let business: Permission = serde_json::from_str(
            r#"{"id":"2","roles":["read"],"link":{"type":"view","scope":"users","webUrl":"https://x"},
                "grantedToIdentitiesV2":[{"user":{"displayName":"Ana","email":"ana@contoso.com"}},
                                         {"user":{"displayName":"Bo","email":"bo@contoso.com"}}],
                "expirationDateTime":"2030-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(business.people().len(), 2);
        assert!(business.expires().is_some());
    }
}
