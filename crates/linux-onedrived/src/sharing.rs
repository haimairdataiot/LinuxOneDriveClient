//! Sharing operations behind the Account D-Bus interface (research doc 05).

use crate::daemon::{AccountRt, Daemon};
use anyhow::{Context, anyhow, bail};
use od_core::dbus::{
    AccessEntry, InviteFailure, InviteOptions, InviteOutcome, LinkOptions, LinkResult, Person, SharingInfo,
    StopSharingOutcome,
};
use od_core::types::DriveType;
use od_engine::Engine;
use od_engine::store;
use od_graph::sharing::{LinkRequest, Permission};
use std::path::Path;
use std::sync::Arc;

const PEOPLE_SCOPE: &str = "People.Read";

fn item_id(engine: &Engine, path: &str) -> anyhow::Result<(String, store::Item)> {
    let it = engine.resolve_abs(Path::new(path)).map_err(|e| anyhow!("{e}"))?;
    let id = it
        .item_id
        .clone()
        .ok_or_else(|| anyhow!("\"{}\" hasn't been uploaded yet. Try again in a moment.", it.name))?;
    Ok((id, it))
}

fn graph_error(e: od_graph::GraphError) -> anyhow::Error {
    let msg = match e.status() {
        Some(403) => "You don't have permission to share this item.".to_string(),
        Some(400) if e.to_string().to_lowercase().contains("anyone") => {
            "Your organization doesn't allow \"Anyone\" links. Try people in your organization or specific people."
                .to_string()
        }
        Some(400) => format!("This kind of sharing isn't available for this account ({}).", e.user_message()),
        _ => e.user_message(),
    };
    anyhow!(msg)
}

fn default_link(d: &Daemon, acct: &AccountRt) -> (String, String) {
    let business = acct.cfg().drive_type != DriveType::Personal;
    match d.cfg().general.default_link.as_str() {
        "anyone-view" => ("anyone".into(), "view".into()),
        "anyone-edit" => ("anyone".into(), "edit".into()),
        "organization-view" if business => ("organization".into(), "view".into()),
        "organization-edit" if business => ("organization".into(), "edit".into()),
        // Microsoft's defaults: personal "Anyone… can edit"; work "People in org… can view".
        _ if business => ("organization".into(), "view".into()),
        _ => ("anyone".into(), "edit".into()),
    }
}

pub fn info(d: &Daemon, acct: &AccountRt, engine: &Engine, path: &str) -> anyhow::Result<SharingInfo> {
    let it = engine.resolve_abs(Path::new(path)).map_err(|e| anyhow!("{e}"))?;
    let cfg = acct.cfg();
    let (audience, role) = default_link(d, acct);
    let (can_share, reason) = if it.ino == store::ROOT_INO {
        (false, "Your whole OneDrive can't be shared. Share a file or folder instead.".to_string())
    } else if it.item_id.is_none() {
        (false, "This item hasn't been uploaded yet.".to_string())
    } else {
        (true, String::new())
    };
    Ok(SharingInfo {
        name: it.name.clone(),
        is_dir: it.is_dir,
        drive_type: cfg.drive_type.as_str().to_string(),
        organization: cfg.organization.clone(),
        can_share,
        reason,
        default_audience: audience,
        default_role: role,
        people_suggestions: cfg.extra_scopes.iter().any(|s| s == PEOPLE_SCOPE),
        web_url: it.web_url.clone().unwrap_or_default(),
    })
}

/// Normalise permissions into "Manage access" rows (doc 05 §B3 algorithm).
pub fn classify(perms: Vec<Permission>) -> Vec<AccessEntry> {
    let mut out = Vec::new();
    for p in perms {
        let inherited_from = p
            .inherited_from
            .as_ref()
            .and_then(|r| r.path.clone())
            .map(|path| path.split(":/").nth(1).map(|s| format!("/{s}")).unwrap_or(path))
            .unwrap_or_default();
        let inherited = p.inherited_from.is_some();
        let people: Vec<Person> = p.people().into_iter().map(|(name, email)| Person { name, email }).collect();
        let expires = p.expires().map(|t| t.timestamp()).unwrap_or(0);
        let has_password = p.has_password.unwrap_or(false);
        if p.roles.iter().any(|r| r == "owner") {
            out.push(AccessEntry {
                id: p.id,
                kind: "owner".into(),
                role: "owner".into(),
                people,
                ..Default::default()
            });
            continue;
        }
        let role_from_roles = if p.roles.iter().any(|r| r == "write") { "edit" } else { "view" };
        match &p.link {
            Some(l) if l.scope.as_deref() == Some("existingAccess") => continue,
            // A link with a type is a real sharing link…
            Some(l) if l.kind.is_some() => {
                let role = match l.kind.as_deref() {
                    Some("edit") => "edit",
                    Some("review") => "review",
                    Some("blocksDownload") => "blocksDownload",
                    _ if l.prevents_download == Some(true) => "blocksDownload",
                    _ => "view",
                };
                let audience = match l.scope.as_deref() {
                    Some("organization") => "organization",
                    Some("users") => "users",
                    _ if !people.is_empty() => "users",
                    _ => "anyone",
                };
                out.push(AccessEntry {
                    id: p.id.clone(),
                    kind: "link".into(),
                    audience: audience.into(),
                    role: role.into(),
                    url: l.web_url.clone().unwrap_or_default(),
                    people,
                    expires,
                    has_password,
                    inherited_from,
                    removable: !inherited,
                    role_editable: false,
                });
            }
            // …while personal invitations come back as untyped links for one person.
            _ => out.push(AccessEntry {
                id: p.id.clone(),
                kind: "person".into(),
                role: role_from_roles.into(),
                url: p.link.as_ref().and_then(|l| l.web_url.clone()).unwrap_or_default(),
                people,
                expires,
                has_password,
                inherited_from,
                removable: !inherited,
                role_editable: !inherited,
                ..Default::default()
            }),
        }
    }
    out
}

pub async fn list_access(engine: &Engine, path: &str) -> anyhow::Result<Vec<AccessEntry>> {
    let (id, _) = item_id(engine, path)?;
    let perms = engine.graph.list_permissions(&id).await.map_err(graph_error)?;
    Ok(classify(perms))
}

fn to_request(o: &LinkOptions) -> anyhow::Result<LinkRequest> {
    let scope = match o.audience.as_str() {
        "anyone" => "anonymous",
        "organization" => "organization",
        "users" => "users",
        other => bail!("unknown link audience {other}"),
    };
    let kind = match o.role.as_str() {
        "view" | "edit" | "review" | "blocksDownload" => o.role.clone(),
        other => bail!("unknown link permission {other}"),
    };
    Ok(LinkRequest {
        kind,
        scope: scope.into(),
        expiration: (o.expires > 0).then(|| chrono::DateTime::from_timestamp(o.expires, 0)).flatten(),
        password: Some(o.password.clone()).filter(|p| !p.is_empty()),
        recipients: o.recipients.clone(),
        send_notification: o.notify,
    })
}

/// Create (or reuse) a link with the requested settings.
pub async fn create_link(
    acct: &AccountRt,
    engine: &Arc<Engine>,
    path: &str,
    o: &LinkOptions,
) -> anyhow::Result<LinkResult> {
    let (id, it) = item_id(engine, path)?;
    if o.audience == "existing" {
        // "People with existing access": just the item's address, no new permission.
        let url = match it.web_url.clone() {
            Some(u) if !u.is_empty() => u,
            _ => engine.graph.item(&id).await.map_err(graph_error)?.web_url.unwrap_or_default(),
        };
        return Ok(LinkResult { url, created: false, settings_applied: true, ..Default::default() });
    }
    // Specific people on personal accounts: an invitation is the link.
    if o.audience == "users" && acct.cfg().drive_type == DriveType::Personal {
        if o.recipients.is_empty() {
            bail!("Add at least one person.");
        }
        let role = if o.role == "edit" { "write" } else { "read" };
        let r = engine.graph.invite(&id, &o.recipients, role, "", o.notify, None, None).await.map_err(graph_error)?;
        let url = r.granted.iter().find_map(|p| p.link.as_ref().and_then(|l| l.web_url.clone())).unwrap_or_default();
        refresh_shared(engine, &it, &id).await;
        let failed = r.failed.iter().map(|(e, _)| e.clone()).collect::<Vec<_>>();
        return Ok(LinkResult {
            url,
            created: true,
            settings_applied: failed.is_empty(),
            permission_id: r.granted.first().map(|p| p.id.clone()).unwrap_or_default(),
            message: if failed.is_empty() { String::new() } else { format!("Couldn't add: {}", failed.join(", ")) },
            ..Default::default()
        });
    }
    let req = to_request(o)?;
    let (mut perm, mut created) = engine.graph.create_link_with(&id, &req).await.map_err(graph_error)?;
    // createLink returns our existing link of the same type and scope, with
    // whatever expiry and password it had. Replacing it would break it for
    // everyone who already has it, so that only happens when asked for.
    let want_expiry = req.expiration.map(|t| t.timestamp());
    let want_password = req.password.is_some();
    let mismatch = |p: &Permission| {
        p.expires().map(|t| t.timestamp()) != want_expiry || p.has_password.unwrap_or(false) != want_password
    };
    let url_of = |p: &Permission| p.link.as_ref().and_then(|l| l.web_url.clone()).unwrap_or_default();
    if !created && mismatch(&perm) {
        if !o.replace {
            return Ok(LinkResult {
                url: url_of(&perm),
                created: false,
                settings_applied: false,
                permission_id: perm.id.clone(),
                message: "A link like this already exists with other settings.".into(),
                existing: true,
                expires: perm.expires().map(|t| t.timestamp()).unwrap_or(0),
                has_password: perm.has_password.unwrap_or(false),
            });
        }
        engine.graph.delete_permission(&id, &perm.id).await.map_err(graph_error)?;
        let (p2, c2) = engine.graph.create_link_with(&id, &req).await.map_err(graph_error)?;
        perm = p2;
        created = c2;
    }
    let applied = !mismatch(&perm);
    refresh_shared(engine, &it, &id).await;
    Ok(LinkResult {
        url: url_of(&perm),
        created,
        settings_applied: applied,
        permission_id: perm.id.clone(),
        message: if applied {
            String::new()
        } else {
            "The expiry date or password couldn't be set. They may need a Microsoft 365 subscription.".into()
        },
        existing: false,
        expires: perm.expires().map(|t| t.timestamp()).unwrap_or(0),
        has_password: perm.has_password.unwrap_or(false),
    })
}

pub async fn copy_link(d: &Daemon, acct: &AccountRt, engine: &Arc<Engine>, path: &str) -> anyhow::Result<String> {
    let (audience, role) = default_link(d, acct);
    let o = LinkOptions { audience, role, ..Default::default() };
    Ok(create_link(acct, engine, path, &o).await?.url)
}

pub async fn invite(engine: &Arc<Engine>, path: &str, o: &InviteOptions) -> anyhow::Result<InviteOutcome> {
    let (id, it) = item_id(engine, path)?;
    let recipients: Vec<String> = o.recipients.iter().map(|r| r.trim().to_string()).filter(|r| !r.is_empty()).collect();
    if recipients.is_empty() {
        bail!("Add at least one name or email address.");
    }
    let role = if o.role == "edit" { "write" } else { "read" };
    let expiration = (o.expires > 0).then(|| chrono::DateTime::from_timestamp(o.expires, 0)).flatten();
    let r = engine
        .graph
        .invite(
            &id,
            &recipients,
            role,
            &o.message,
            o.notify,
            expiration,
            Some(o.password.as_str()).filter(|p| !p.is_empty()),
        )
        .await
        .map_err(graph_error)?;
    remember_recipients(engine, &recipients);
    refresh_shared(engine, &it, &id).await;
    Ok(InviteOutcome {
        granted: r.granted.len() as u32,
        failed: r
            .failed
            .into_iter()
            .map(|(email, e)| InviteFailure {
                email,
                message: e.localized_message.unwrap_or(e.message),
                fix_url: e.fix_it_url.unwrap_or_default(),
            })
            .collect(),
    })
}

pub async fn set_role(engine: &Arc<Engine>, path: &str, perm: &str, role: &str) -> anyhow::Result<()> {
    let (id, _) = item_id(engine, path)?;
    let r = if role == "edit" { "write" } else { "read" };
    engine.graph.update_permission_roles(&id, perm, r).await.map_err(graph_error)?;
    Ok(())
}

pub async fn remove(engine: &Arc<Engine>, path: &str, perm: &str) -> anyhow::Result<()> {
    let (id, it) = item_id(engine, path)?;
    engine.graph.delete_permission(&id, perm).await.map_err(graph_error)?;
    refresh_shared(engine, &it, &id).await;
    Ok(())
}

pub async fn remove_people(engine: &Arc<Engine>, path: &str, perm: &str, emails: Vec<String>) -> anyhow::Result<()> {
    let (id, _) = item_id(engine, path)?;
    engine.graph.revoke_grants(&id, perm, &emails).await.map_err(graph_error)?;
    Ok(())
}

pub async fn stop_sharing(engine: &Arc<Engine>, path: &str) -> anyhow::Result<StopSharingOutcome> {
    let (id, it) = item_id(engine, path)?;
    let entries = classify(engine.graph.list_permissions(&id).await.map_err(graph_error)?);
    let mut out = StopSharingOutcome::default();
    for e in entries.iter().filter(|e| e.kind != "owner") {
        if !e.inherited_from.is_empty() {
            out.failed.push(format!("Access through \"{}\" must be changed on that folder.", e.inherited_from));
            continue;
        }
        match engine.graph.delete_permission(&id, &e.id).await {
            Ok(()) => out.removed += 1,
            Err(err) => out.failed.push(graph_error(err).to_string()),
        }
    }
    refresh_shared(engine, &it, &id).await;
    Ok(out)
}

fn remember_recipients(engine: &Engine, emails: &[String]) {
    let c = engine.store.lock();
    let mut list: Vec<String> = store::kv_get(&c, "recent_recipients")
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    for e in emails {
        list.retain(|x| !x.eq_ignore_ascii_case(e));
        list.insert(0, e.clone());
    }
    list.truncate(30);
    let _ = store::kv_set(&c, "recent_recipients", &serde_json::to_string(&list).unwrap_or_default());
}

pub async fn search_people(acct: &AccountRt, engine: &Engine, query: &str) -> Vec<Person> {
    let q = query.trim().to_lowercase();
    let recent: Vec<String> = store::kv_get(&engine.store.lock(), "recent_recipients")
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let mut out: Vec<Person> = recent
        .into_iter()
        .filter(|e| q.is_empty() || e.to_lowercase().contains(&q))
        .map(|email| Person { name: String::new(), email })
        .collect();
    if q.chars().count() >= 2 && acct.cfg().extra_scopes.iter().any(|s| s == PEOPLE_SCOPE) {
        if let Ok(hits) = engine.graph.search_people(&q).await {
            for h in hits {
                if !out.iter().any(|p| p.email.eq_ignore_ascii_case(&h.email)) {
                    out.push(Person { name: h.name, email: h.email });
                }
            }
        }
    }
    out.truncate(10);
    out
}

/// After a sharing change: refresh the item's "shared" state (people emblem).
async fn refresh_shared(engine: &Engine, it: &store::Item, id: &str) {
    if let Ok((shared, _)) = engine.graph.sharing_state(id).await {
        engine.set_shared(it.ino, shared);
    }
}

/// Incremental consent for people suggestions.
pub async fn begin_people_consent(d: &Arc<Daemon>, acct: &AccountRt) -> anyhow::Result<(String, String)> {
    let email = acct.cfg().email;
    d.begin_consent(&acct.id, &email, &[PEOPLE_SCOPE]).await.context("starting consent")
}
