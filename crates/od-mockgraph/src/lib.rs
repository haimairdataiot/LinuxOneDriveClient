//! An in-memory fake of the Microsoft Graph OneDrive API, good enough to
//! exercise the sync engine end to end: delta with paging and tokens,
//! path addressing, downloads with redirects and Range, simple uploads,
//! upload sessions, folders, rename/move, delete, eTag preconditions,
//! sharing links, versions and fault injection.
//!
//! Tests also use it as "the other device": helper methods change the remote
//! drive directly (`put_file`, `mkdir`, `remove`, `rename`), producing delta
//! entries exactly like edits made on the web.

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use od_core::quickxor;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

pub const DRIVE_ID: &str = "mockdrive0001";
pub const ROOT_ID: &str = "ROOT";
pub const VALID_TOKEN: &str = "mock-access-token";

#[derive(Debug, Clone)]
pub struct MockItem {
    pub id: String,
    pub name: String,
    pub parent: Option<String>,
    pub folder: bool,
    pub content: Bytes,
    pub etag_ver: u64,
    pub ctag_ver: u64,
    pub mtime: DateTime<Utc>,
    pub ctime: DateTime<Utc>,
    pub deleted: bool,
    pub shared: bool,
    /// Change sequence number of the last modification (for delta).
    pub seq: u64,
    pub versions: Vec<(String, Bytes, DateTime<Utc>)>,
    /// A shortcut ("Add to My files"): (drive id, item id) of the shared folder.
    pub remote: Option<(String, String)>,
}

/// A drive other than the signed-in user's: someone else's OneDrive (the
/// source of a shared folder) or a SharePoint document library.
#[derive(Debug, Clone)]
pub struct MockDrive {
    pub id: String,
    /// Item id of the drive's root folder.
    pub root: String,
    /// "personal" | "business" | "documentLibrary"
    pub drive_type: String,
    pub name: String,
    /// SharePoint site (id, display name) for libraries.
    pub site: Option<(String, String)>,
    /// SharePoint list id of a library (what the browser's Sync button sends).
    pub list_id: String,
}

impl MockItem {
    fn etag(&self) -> String {
        format!("\"{{{}}},{}\"", self.id, self.etag_ver)
    }
    fn ctag(&self) -> String {
        format!("\"c:{{{}}},{}\"", self.id, self.ctag_ver)
    }
}

/// How an injected fault misbehaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FaultKind {
    /// Fail without doing anything.
    #[default]
    Error,
    /// Do the work (e.g. commit an upload), then answer with an error: a lost reply.
    AfterCommit,
    /// Serve download content with flipped bytes.
    CorruptBody,
    /// Serve only half of the download content while announcing the full length.
    TruncateBody,
}

/// A fault to inject into matching requests.
#[derive(Debug, Clone)]
pub struct Fault {
    pub method: Option<Method>,
    pub path_contains: String,
    pub status: u16,
    pub code: String,
    pub retry_after: Option<u64>,
    pub times: u32,
    pub kind: FaultKind,
    /// Let this many matching requests through before faulting.
    pub skip: u32,
}

#[derive(Debug)]
struct UploadSession {
    target_parent: Option<String>,
    target_name: Option<String>,
    target_item: Option<String>,
    conflict: String,
    received: Vec<u8>,
    total: Option<u64>,
    mtime: Option<DateTime<Utc>>,
}

#[derive(Debug, Default)]
pub struct Stats {
    pub requests: u64,
    pub downloads: u64,
    pub uploads_completed: u64,
    pub deletes: u64,
    pub delta_calls: u64,
    pub bytes_uploaded: u64,
    pub bytes_downloaded: u64,
    pub ranged_requests: u64,
    pub thumbnails: u64,
}

pub struct DriveState {
    pub items: HashMap<String, MockItem>,
    seq: u64,
    next_id: u64,
    sessions: HashMap<String, UploadSession>,
    faults: VecDeque<Fault>,
    /// Delta tokens below this are rejected with 410 (simulates expiry).
    pub min_valid_token: u64,
    pub delta_page_size: usize,
    pub quota_total: u64,
    pub stats: Stats,
    base_url: String,
    /// Bumped on every change; WebSocket clients get a notification.
    changes: tokio::sync::watch::Sender<u64>,
    /// Offer the real-time notification endpoint.
    pub push_enabled: bool,
    /// Sharing permissions by item id (the owner entry is implicit).
    pub permissions: HashMap<String, Vec<Value>>,
    next_perm: u64,
    /// Tenant policy: allow "Anyone" links.
    pub allow_anonymous_links: bool,
    /// Other drives (shared folders' sources, SharePoint libraries).
    pub drives: Vec<MockDrive>,
    /// Account type of the signed-in user's drive.
    pub my_drive_type: String,
    /// Name and address of the signed-in user.
    pub user_name: String,
    pub user_email: String,
    /// Offer delta on folders of business drives (SharePoint Online does;
    /// older services and national clouds only offer it on the root).
    pub business_folder_delta: bool,
    /// The "latest release" the update check sees (GitHub releases API).
    pub release: Option<MockRelease>,
}

/// A published app release, as GitHub's API would describe it.
#[derive(Debug, Clone)]
pub struct MockRelease {
    pub tag: String,
    pub notes: String,
    pub asset_name: String,
    pub package: Bytes,
    /// Publish a SHA256SUMS file.
    pub with_checksums: bool,
    /// Make the checksum wrong (a tampered or damaged download).
    pub wrong_checksum: bool,
    pub prerelease: bool,
}

impl DriveState {
    fn new() -> Self {
        let now = Utc::now();
        let mut items = HashMap::new();
        items.insert(
            ROOT_ID.to_string(),
            MockItem {
                id: ROOT_ID.into(),
                name: "root".into(),
                parent: None,
                folder: true,
                content: Bytes::new(),
                etag_ver: 1,
                ctag_ver: 1,
                mtime: now,
                ctime: now,
                deleted: false,
                shared: false,
                seq: 1,
                versions: vec![],
                remote: None,
            },
        );
        Self {
            items,
            seq: 1,
            next_id: 1,
            sessions: HashMap::new(),
            faults: VecDeque::new(),
            min_valid_token: 0,
            delta_page_size: 200,
            quota_total: 5 * 1024 * 1024 * 1024,
            stats: Stats::default(),
            base_url: String::new(),
            changes: tokio::sync::watch::channel(0).0,
            push_enabled: true,
            permissions: HashMap::new(),
            next_perm: 0,
            allow_anonymous_links: true,
            drives: Vec::new(),
            my_drive_type: "personal".into(),
            user_name: "Mock User".into(),
            user_email: "mock.user@example.com".into(),
            business_folder_delta: false,
            release: None,
        }
    }

    /// The root folder an item belongs to.
    fn top_of(&self, id: &str) -> String {
        let mut cur = id.to_string();
        while let Some(p) = self.items.get(&cur).and_then(|i| i.parent.clone()) {
            cur = p;
        }
        cur
    }

    /// (drive id, drive type) of an item.
    fn drive_of(&self, id: &str) -> (String, String) {
        let top = self.top_of(id);
        match self.drives.iter().find(|d| d.root == top) {
            Some(d) => (d.id.clone(), d.drive_type.clone()),
            None => (DRIVE_ID.to_string(), self.my_drive_type.clone()),
        }
    }

    fn drive_root(&self, drive_id: &str) -> Option<String> {
        if drive_id.eq_ignore_ascii_case(DRIVE_ID) {
            return Some(ROOT_ID.to_string());
        }
        self.drives.iter().find(|d| d.id == drive_id).map(|d| d.root.clone())
    }

    /// Is `id` the folder `scope` or inside it?
    fn is_under(&self, id: &str, scope: &str) -> bool {
        let mut cur = Some(id.to_string());
        while let Some(c) = cur {
            if c == scope {
                return true;
            }
            cur = self.items.get(&c).and_then(|i| i.parent.clone());
        }
        false
    }

    fn bump(&mut self) -> u64 {
        self.seq += 1;
        self.changes.send_replace(self.seq);
        self.seq
    }

    fn new_id(&mut self) -> String {
        self.next_id += 1;
        format!("ITEM{:06}", self.next_id)
    }

    fn live(&self, id: &str) -> Option<&MockItem> {
        self.items.get(id).filter(|i| !i.deleted)
    }

    fn child(&self, parent: &str, name: &str) -> Option<&MockItem> {
        let lname = name.to_lowercase();
        self.items
            .values()
            .find(|i| !i.deleted && i.parent.as_deref() == Some(parent) && i.name.to_lowercase() == lname)
    }

    /// Resolve "/a/b/c" (relative to root) to an id.
    pub fn resolve(&self, path: &str) -> Option<String> {
        self.resolve_in(ROOT_ID, path)
    }

    /// Resolve a path relative to the folder `root`.
    pub fn resolve_in(&self, root: &str, path: &str) -> Option<String> {
        let mut cur = root.to_string();
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            cur = self.child(&cur, seg)?.id.clone();
        }
        Some(cur)
    }

    fn path_of(&self, id: &str) -> String {
        let mut parts = vec![];
        let mut cur = self.items.get(id);
        while let Some(i) = cur {
            if i.parent.is_none() {
                break;
            }
            parts.push(i.name.clone());
            cur = i.parent.as_ref().and_then(|p| self.items.get(p));
        }
        parts.reverse();
        format!("/{}", parts.join("/"))
    }

    fn item_json(&self, i: &MockItem) -> Value {
        let mut v = json!({
            "id": i.id,
            "name": i.name,
            "eTag": i.etag(),
            "cTag": i.ctag(),
            "createdDateTime": i.ctime.to_rfc3339(),
            "lastModifiedDateTime": i.mtime.to_rfc3339(),
            "fileSystemInfo": {
                "createdDateTime": i.ctime.to_rfc3339(),
                "lastModifiedDateTime": i.mtime.to_rfc3339(),
            },
            "webUrl": format!("https://onedrive.example/{}", i.id),
            "size": if i.folder { 0 } else { i.content.len() },
        });
        if !i.folder && i.id != ROOT_ID {
            v["@microsoft.graph.downloadUrl"] = json!(format!("{}/_download/{}", self.base_url, i.id));
        }
        let (drive_id, drive_type) = self.drive_of(&i.id);
        if i.parent.is_none() {
            v["root"] = json!({});
            v["folder"] = json!({ "childCount": 0 });
        } else if let Some((rd, rid)) = &i.remote {
            // Shortcut to a shared folder: a placeholder in this drive.
            let (rtype, rname, count) = match self.items.get(rid) {
                Some(t) => (
                    self.drive_of(rid).1,
                    t.name.clone(),
                    self.items.values().filter(|c| !c.deleted && c.parent.as_deref() == Some(rid)).count(),
                ),
                None => ("personal".into(), i.name.clone(), 0),
            };
            v["folder"] = json!({ "childCount": count });
            v["remoteItem"] = json!({
                "id": rid,
                "name": rname,
                "folder": { "childCount": count },
                "parentReference": { "driveId": rd, "driveType": rtype },
                "webUrl": format!("https://onedrive.example/{rid}"),
            });
        } else if i.folder {
            let count = self.items.values().filter(|c| !c.deleted && c.parent.as_deref() == Some(&i.id)).count();
            v["folder"] = json!({ "childCount": count });
        } else {
            v["file"] = json!({
                "mimeType": "application/octet-stream",
                "hashes": { "quickXorHash": quickxor_b64(&i.content) },
            });
        }
        if let Some(p) = &i.parent {
            v["parentReference"] = json!({ "driveId": drive_id, "driveType": drive_type, "id": p });
        }
        if i.shared {
            v["shared"] = json!({ "scope": "users" });
        }
        if i.deleted {
            v["deleted"] = json!({ "state": "deleted" });
        }
        v
    }

    fn take_fault(&mut self, method: &Method, path: &str) -> Option<Fault> {
        let idx = self.faults.iter().position(|f| {
            f.times > 0 && path.contains(&f.path_contains) && f.method.as_ref().is_none_or(|m| m == method)
        })?;
        let f = &mut self.faults[idx];
        if f.skip > 0 {
            f.skip -= 1;
            return None;
        }
        f.times -= 1;
        let out = f.clone();
        if f.times == 0 {
            self.faults.remove(idx);
        }
        Some(out)
    }

    // -------- mutations shared by HTTP handlers and test helpers --------

    fn create_file(&mut self, parent: &str, name: &str, content: Bytes, mtime: Option<DateTime<Utc>>) -> String {
        let seq = self.bump();
        let now = Utc::now();
        let id = self.new_id();
        self.items.insert(
            id.clone(),
            MockItem {
                id: id.clone(),
                name: name.into(),
                parent: Some(parent.into()),
                folder: false,
                content,
                etag_ver: 1,
                ctag_ver: 1,
                mtime: mtime.unwrap_or(now),
                ctime: now,
                deleted: false,
                shared: false,
                seq,
                versions: vec![],
                remote: None,
            },
        );
        self.touch_ancestors(parent, seq);
        id
    }

    fn replace_content(&mut self, id: &str, content: Bytes, mtime: Option<DateTime<Utc>>) {
        let seq = self.bump();
        let parent = {
            let it = self.items.get_mut(id).expect("item");
            let old = std::mem::replace(&mut it.content, content);
            let vid = format!("{}.0", it.versions.len() + 1);
            it.versions.push((vid, old, it.mtime));
            it.etag_ver += 1;
            it.ctag_ver += 1;
            it.mtime = mtime.unwrap_or_else(Utc::now);
            it.seq = seq;
            it.parent.clone()
        };
        if let Some(p) = parent {
            self.touch_ancestors(&p, seq);
        }
    }

    fn create_folder(&mut self, parent: &str, name: &str) -> String {
        let seq = self.bump();
        let now = Utc::now();
        let id = self.new_id();
        self.items.insert(
            id.clone(),
            MockItem {
                id: id.clone(),
                name: name.into(),
                parent: Some(parent.into()),
                folder: true,
                content: Bytes::new(),
                etag_ver: 1,
                ctag_ver: 1,
                mtime: now,
                ctime: now,
                deleted: false,
                shared: false,
                seq,
                versions: vec![],
                remote: None,
            },
        );
        self.touch_ancestors(parent, seq);
        id
    }

    /// Folder cTags change when descendants change (as on OneDrive).
    fn touch_ancestors(&mut self, start: &str, seq: u64) {
        let mut cur = Some(start.to_string());
        while let Some(id) = cur {
            match self.items.get_mut(&id) {
                Some(it) => {
                    it.ctag_ver += 1;
                    it.seq = seq;
                    cur = it.parent.clone();
                }
                None => break,
            }
        }
    }

    fn delete_recursive(&mut self, id: &str) {
        let seq = self.bump();
        let children: Vec<String> = self
            .items
            .values()
            .filter(|c| !c.deleted && c.parent.as_deref() == Some(id))
            .map(|c| c.id.clone())
            .collect();
        for c in children {
            self.delete_recursive(&c);
        }
        let parent = {
            let it = self.items.get_mut(id).expect("item");
            it.deleted = true;
            it.seq = seq;
            it.parent.clone()
        };
        if let Some(p) = parent {
            self.touch_ancestors(&p, seq);
        }
    }

    fn rename_move(&mut self, id: &str, new_name: Option<String>, new_parent: Option<String>) {
        let seq = self.bump();
        let old_parent = {
            let it = self.items.get_mut(id).expect("item");
            let old_parent = it.parent.clone();
            if let Some(n) = new_name {
                it.name = n;
            }
            if let Some(p) = new_parent {
                it.parent = Some(p);
            }
            it.etag_ver += 1;
            it.seq = seq;
            old_parent
        };
        if let Some(p) = old_parent {
            self.touch_ancestors(&p, seq);
        }
        let np = self.items[id].parent.clone();
        if let Some(p) = np {
            self.touch_ancestors(&p, seq);
        }
    }
}

pub fn quickxor_b64(data: &[u8]) -> String {
    quickxor::hash_bytes(data)
}

/// Handle to a running mock server.
#[derive(Clone)]
pub struct MockGraph {
    pub state: Arc<Mutex<DriveState>>,
    pub addr: SocketAddr,
}

impl MockGraph {
    /// Start on 127.0.0.1 with a random port.
    pub async fn start() -> MockGraph {
        let state = Arc::new(Mutex::new(DriveState::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        state.lock().unwrap().base_url = format!("http://{addr}");
        let app = Router::new()
            .route("/socket.io/", axum::routing::get(socket_io))
            .fallback(handle)
            .layer(axum::extract::DefaultBodyLimit::max(512 * 1024 * 1024))
            .with_state(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        MockGraph { state, addr }
    }

    /// Base URL to pass as the Graph base.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    // ------------------------------------------------ "other device" helpers

    /// Create or overwrite a file at `path`, creating parent folders.
    pub fn put_file(&self, path: &str, content: &[u8]) -> String {
        let mut s = self.state.lock().unwrap();
        let (dir, name) = split_path(path);
        let parent = ensure_dirs(&mut s, dir);
        if let Some(existing) = s.child(&parent, name).map(|i| i.id.clone()) {
            s.replace_content(&existing, Bytes::copy_from_slice(content), None);
            existing
        } else {
            s.create_file(&parent, name, Bytes::copy_from_slice(content), None)
        }
    }

    pub fn mkdir(&self, path: &str) -> String {
        let mut s = self.state.lock().unwrap();
        ensure_dirs(&mut s, path)
    }

    // ------------------------------------------------ other drives, shortcuts, sites

    /// Publish (or withdraw) the release the update check finds.
    pub fn set_release(&self, release: Option<MockRelease>) {
        self.state.lock().unwrap().release = release;
    }

    /// Offer delta on folders of business drives.
    pub fn set_business_folder_delta(&self, on: bool) {
        self.state.lock().unwrap().business_folder_delta = on;
    }

    /// Name and address of the signed-in user (demos and screenshots).
    pub fn set_user(&self, name: &str, email: &str) {
        let mut s = self.state.lock().unwrap();
        s.user_name = name.to_string();
        s.user_email = email.to_string();
    }

    /// The signed-in user has a work or school account.
    pub fn set_business(&self, on: bool) {
        self.state.lock().unwrap().my_drive_type = if on { "business" } else { "personal" }.into();
    }

    /// Add another drive (someone else's OneDrive, or a SharePoint library
    /// when `site` is given); returns its root item id.
    pub fn add_drive(&self, id: &str, drive_type: &str, name: &str, site: Option<(&str, &str)>) -> String {
        let mut s = self.state.lock().unwrap();
        let seq = s.bump();
        let now = Utc::now();
        let root = format!("ROOT-{id}");
        s.items.insert(
            root.clone(),
            MockItem {
                id: root.clone(),
                name: "root".into(),
                parent: None,
                folder: true,
                content: Bytes::new(),
                etag_ver: 1,
                ctag_ver: 1,
                mtime: now,
                ctime: now,
                deleted: false,
                shared: false,
                seq,
                versions: vec![],
                remote: None,
            },
        );
        s.drives.push(MockDrive {
            id: id.into(),
            root: root.clone(),
            drive_type: drive_type.into(),
            name: name.into(),
            site: site.map(|(a, b)| (a.to_string(), b.to_string())),
            list_id: format!("{:08x}-0000-4000-8000-{:012x}", seq, seq),
        });
        root
    }

    /// SharePoint list id of a library drive (for `odopen://` links).
    pub fn list_id(&self, drive_id: &str) -> String {
        let s = self.state.lock().unwrap();
        s.drives.iter().find(|d| d.id == drive_id).map(|d| d.list_id.clone()).unwrap_or_default()
    }

    fn root_of(s: &DriveState, drive_id: &str) -> String {
        s.drive_root(drive_id).unwrap_or_else(|| panic!("unknown drive {drive_id}"))
    }

    pub fn put_file_in(&self, drive_id: &str, path: &str, content: &[u8]) -> String {
        let mut s = self.state.lock().unwrap();
        let root = Self::root_of(&s, drive_id);
        let (dir, name) = split_path(path);
        let parent = ensure_dirs_in(&mut s, &root, dir);
        if let Some(existing) = s.child(&parent, name).map(|i| i.id.clone()) {
            s.replace_content(&existing, Bytes::copy_from_slice(content), None);
            existing
        } else {
            s.create_file(&parent, name, Bytes::copy_from_slice(content), None)
        }
    }

    pub fn mkdir_in(&self, drive_id: &str, path: &str) -> String {
        let mut s = self.state.lock().unwrap();
        let root = Self::root_of(&s, drive_id);
        ensure_dirs_in(&mut s, &root, path)
    }

    pub fn read_in(&self, drive_id: &str, path: &str) -> Option<Vec<u8>> {
        let s = self.state.lock().unwrap();
        let id = s.resolve_in(&Self::root_of(&s, drive_id), path)?;
        let it = s.live(&id)?;
        (!it.folder).then(|| it.content.to_vec())
    }

    pub fn list_in(&self, drive_id: &str, path: &str) -> Vec<String> {
        let s = self.state.lock().unwrap();
        let Some(id) = s.resolve_in(&Self::root_of(&s, drive_id), path) else { return vec![] };
        let mut names: Vec<String> = s
            .items
            .values()
            .filter(|i| !i.deleted && i.parent.as_deref() == Some(id.as_str()))
            .map(|i| i.name.clone())
            .collect();
        names.sort();
        names
    }

    pub fn remove_in(&self, drive_id: &str, path: &str) -> bool {
        let mut s = self.state.lock().unwrap();
        let root = Self::root_of(&s, drive_id);
        match s.resolve_in(&root, path) {
            Some(id) if id != root => {
                s.delete_recursive(&id);
                true
            }
            _ => false,
        }
    }

    /// "Add shortcut to My files": a placeholder in the user's root that
    /// points at folder `target` of drive `drive_id`. Returns its id.
    pub fn add_shortcut(&self, name: &str, drive_id: &str, target: &str) -> String {
        let mut s = self.state.lock().unwrap();
        let id = s.create_folder(ROOT_ID, name);
        s.items.get_mut(&id).unwrap().remote = Some((drive_id.to_string(), target.to_string()));
        id
    }

    pub fn remove(&self, path: &str) -> bool {
        let mut s = self.state.lock().unwrap();
        match s.resolve(path) {
            Some(id) if id != ROOT_ID => {
                s.delete_recursive(&id);
                true
            }
            _ => false,
        }
    }

    /// Rename and/or move: `to` is the new full path (parent must exist).
    pub fn rename(&self, from: &str, to: &str) -> bool {
        let mut s = self.state.lock().unwrap();
        let Some(id) = s.resolve(from) else { return false };
        let (dir, name) = split_path(to);
        let Some(parent) = s.resolve(dir) else { return false };
        s.rename_move(&id, Some(name.to_string()), Some(parent));
        true
    }

    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        let s = self.state.lock().unwrap();
        let id = s.resolve(path)?;
        let it = s.live(&id)?;
        (!it.folder).then(|| it.content.to_vec())
    }

    pub fn exists(&self, path: &str) -> bool {
        let s = self.state.lock().unwrap();
        s.resolve(path).is_some()
    }

    pub fn id_of(&self, path: &str) -> Option<String> {
        self.state.lock().unwrap().resolve(path)
    }

    /// Sorted names in a folder.
    pub fn list(&self, path: &str) -> Vec<String> {
        let s = self.state.lock().unwrap();
        let Some(id) = s.resolve(path) else { return vec![] };
        let mut names: Vec<String> = s
            .items
            .values()
            .filter(|i| !i.deleted && i.parent.as_deref() == Some(id.as_str()))
            .map(|i| i.name.clone())
            .collect();
        names.sort();
        names
    }

    /// All live file paths with content (for whole-drive assertions).
    pub fn snapshot(&self) -> std::collections::BTreeMap<String, Option<Vec<u8>>> {
        let s = self.state.lock().unwrap();
        s.items
            .values()
            .filter(|i| !i.deleted && i.id != ROOT_ID && s.is_under(&i.id, ROOT_ID))
            .map(|i| (s.path_of(&i.id), (!i.folder).then(|| i.content.to_vec())))
            .collect()
    }

    pub fn inject(&self, fault: Fault) {
        self.state.lock().unwrap().faults.push_back(fault);
    }

    /// Simple fault: fail the next `times` requests whose path contains `path` with `status`.
    pub fn fail(&self, path_contains: &str, status: u16, times: u32) {
        self.inject(Fault {
            method: None,
            path_contains: path_contains.into(),
            status,
            code: if status == 429 { "activityLimitReached".into() } else { "serviceNotAvailable".into() },
            retry_after: if status == 429 || status == 503 { Some(1) } else { None },
            times,
            kind: FaultKind::Error,
            skip: 0,
        });
    }

    /// Inject a misbehaving fault of a given kind.
    pub fn fault(&self, path_contains: &str, kind: FaultKind, times: u32) {
        self.inject(Fault {
            method: None,
            path_contains: path_contains.into(),
            status: 500,
            code: "generalException".into(),
            retry_after: None,
            times,
            kind,
            skip: 0,
        });
    }

    /// Fault the matching request after letting `skip` of them through.
    pub fn fault_after(&self, path_contains: &str, kind: FaultKind, status: u16, skip: u32) {
        self.inject(Fault {
            method: None,
            path_contains: path_contains.into(),
            status,
            code: "invalidRequest".into(),
            retry_after: None,
            times: 1,
            kind,
            skip,
        });
    }

    /// Remove every pending injected fault.
    pub fn clear_faults(&self) {
        self.state.lock().unwrap().faults.clear();
    }

    /// Invalidate all outstanding delta tokens (next delta call gets 410).
    pub fn expire_delta_tokens(&self) {
        let mut s = self.state.lock().unwrap();
        s.min_valid_token = s.seq + 1;
    }

    /// Sharing permissions stored for a path (not counting the owner).
    pub fn permissions(&self, path: &str) -> Vec<Value> {
        let s = self.state.lock().unwrap();
        s.resolve(path).and_then(|id| s.permissions.get(&id).cloned()).unwrap_or_default()
    }

    pub fn set_allow_anonymous_links(&self, on: bool) {
        self.state.lock().unwrap().allow_anonymous_links = on;
    }

    pub fn set_push_enabled(&self, on: bool) {
        self.state.lock().unwrap().push_enabled = on;
    }

    pub fn set_delta_page_size(&self, n: usize) {
        self.state.lock().unwrap().delta_page_size = n.max(1);
    }

    pub fn stat(&self, f: impl FnOnce(&Stats) -> u64) -> u64 {
        f(&self.state.lock().unwrap().stats)
    }
}

fn split_path(path: &str) -> (&str, &str) {
    let p = path.trim_end_matches('/');
    match p.rfind('/') {
        Some(i) => (&p[..i], &p[i + 1..]),
        None => ("", p),
    }
}

fn ensure_dirs(s: &mut DriveState, path: &str) -> String {
    ensure_dirs_in(s, ROOT_ID, path)
}

fn ensure_dirs_in(s: &mut DriveState, root: &str, path: &str) -> String {
    let mut cur = root.to_string();
    for seg in path.split('/').filter(|x| !x.is_empty()) {
        cur = match s.child(&cur, seg).map(|i| (i.id.clone(), i.folder)) {
            Some((id, true)) => id,
            Some((_, false)) => panic!("{seg} is a file"),
            None => s.create_folder(&cur, seg),
        };
    }
    cur
}

// ------------------------------------------------------------------ HTTP

fn err(status: StatusCode, code: &str, msg: &str) -> Response {
    (status, axum::Json(json!({ "error": { "code": code, "message": msg } }))).into_response()
}

fn ok_json(v: Value) -> Response {
    (StatusCode::OK, axum::Json(v)).into_response()
}

fn query_map(uri: &Uri) -> HashMap<String, String> {
    uri.query().map(|q| url::form_urlencoded::parse(q.as_bytes()).into_owned().collect()).unwrap_or_default()
}

fn pct_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={}", s.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

fn if_match_ok(headers: &HeaderMap, item: &MockItem) -> bool {
    match headers.get(header::IF_MATCH).and_then(|v| v.to_str().ok()) {
        None => true,
        Some("*") => true,
        Some(tag) => tag == item.etag() || tag == item.ctag(),
    }
}

async fn handle(
    State(state): State<Arc<Mutex<DriveState>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();
    let query = query_map(&uri);
    let mut s = state.lock().unwrap();
    s.stats.requests += 1;

    // Identity platform emulation. The authorize page "signs in" instantly by
    // redirecting back with a code (used by demos and GUI tests).
    if method == Method::GET && path.ends_with("/oauth2/v2.0/authorize") {
        let redirect = query.get("redirect_uri").cloned().unwrap_or_default();
        let state = query.get("state").cloned().unwrap_or_default();
        let sep = if redirect.contains('?') { '&' } else { '?' };
        let mut resp = StatusCode::FOUND.into_response();
        resp.headers_mut()
            .insert(header::LOCATION, format!("{redirect}{sep}code=mock-code&state={state}").parse().unwrap());
        return resp;
    }
    if method == Method::GET && path.ends_with("/oauth2/nativeclient") {
        return (StatusCode::OK, "Signed in").into_response();
    }
    // Any code / refresh token yields valid tokens.
    if method == Method::POST && path.ends_with("/oauth2/v2.0/token") {
        use base64::Engine as _;
        let claims = serde_json::json!({
            "tid": "9188040d-6c67-4c5b-b112-36a304b66dad",
            "name": s.user_name,
            "preferred_username": s.user_email
        });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
        return ok_json(json!({
            "token_type": "Bearer",
            "scope": "Files.ReadWrite.All User.Read offline_access",
            "expires_in": 3600,
            "access_token": VALID_TOKEN,
            "refresh_token": "mock-refresh-token",
            "id_token": format!("eyJhbGciOiJub25lIn0.{payload}.sig"),
        }));
    }
    // GitHub releases API emulation (the app's update check); no sign-in needed.
    if method == Method::GET && path.starts_with("/repos/") && path.ends_with("/releases/latest") {
        let Some(r) = s.release.clone() else {
            return err(StatusCode::NOT_FOUND, "Not Found", "no releases");
        };
        let base = s.base_url.clone();
        let mut assets = vec![json!({
            "name": r.asset_name,
            "browser_download_url": format!("{base}/_release/{}", r.asset_name),
            "size": r.package.len(),
        })];
        if r.with_checksums {
            assets.push(json!({ "name": "SHA256SUMS", "browser_download_url": format!("{base}/_release/SHA256SUMS"), "size": 100 }));
        }
        return ok_json(json!({
            "tag_name": r.tag,
            "name": r.tag,
            "body": r.notes,
            "html_url": format!("{base}/releases/tag/{}", r.tag),
            "published_at": Utc::now().to_rfc3339(),
            "draft": false,
            "prerelease": r.prerelease,
            "assets": assets,
        }));
    }
    if method == Method::GET
        && let Some(name) = path.strip_prefix("/_release/")
    {
        let Some(r) = s.release.clone() else {
            return err(StatusCode::NOT_FOUND, "Not Found", "no releases");
        };
        if name == "SHA256SUMS" && r.with_checksums {
            use sha2::Digest as _;
            let mut hash = format!("{:x}", sha2::Sha256::digest(&r.package));
            if r.wrong_checksum {
                hash = "0".repeat(64);
            }
            return (StatusCode::OK, format!("{hash}  {}\n", r.asset_name)).into_response();
        }
        if name == r.asset_name {
            return (StatusCode::OK, r.package).into_response();
        }
        return err(StatusCode::NOT_FOUND, "Not Found", "no such asset");
    }
    // Server-side thumbnail: an 800x600 PNG whose colour depends on the item.
    if let Some(id) = path.strip_prefix("/_thumb/") {
        s.stats.thumbnails += 1;
        let shade = id.bytes().fold(0u8, |a, b| a.wrapping_add(b));
        let mut buf = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut buf, 800, 600);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[shade, 120, 200].repeat(800 * 600)).unwrap();
        }
        let mut resp = (StatusCode::OK, buf).into_response();
        resp.headers_mut().insert(header::CONTENT_TYPE, "image/png".parse().unwrap());
        return resp;
    }
    // Pre-authenticated endpoints (no bearer token).
    if let Some(rest) = path.strip_prefix("/_download/") {
        let fault = s.take_fault(&method, &path);
        if let Some(f) = fault.as_ref().filter(|f| f.kind == FaultKind::Error) {
            return err(StatusCode::from_u16(f.status).unwrap(), &f.code, "injected fault");
        }
        return download(&mut s, rest, &headers, fault.map(|f| f.kind));
    }
    if let Some(token) = path.strip_prefix("/_upload/") {
        return upload_session_request(&mut s, &method, token, &headers, body);
    }

    let authorized = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {VALID_TOKEN}"));
    if !authorized {
        return err(StatusCode::UNAUTHORIZED, "InvalidAuthenticationToken", "bad token");
    }

    if let Some(f) = s.take_fault(&method, &path) {
        let mut resp = err(StatusCode::from_u16(f.status).unwrap(), &f.code, "injected fault");
        if let Some(ra) = f.retry_after {
            resp.headers_mut().insert(header::RETRY_AFTER, ra.to_string().parse().unwrap());
        }
        return resp;
    }

    // Normalise /drives/{id}/... to /me/drive/... (item ids are unique across drives).
    let p = normalize_drive_path(&s, &path);
    if let Some(rest) = p.strip_prefix("/me/drive/items/")
        && let Some(id) = rest.strip_suffix("/delta")
        && !id.contains('/')
    {
        let id = id.to_string();
        if s.live(&id).is_none() {
            return err(StatusCode::NOT_FOUND, "itemNotFound", "no such folder");
        }
        let (drive, dtype) = s.drive_of(&id);
        if dtype != "personal" && !s.business_folder_delta && s.items.get(&id).is_some_and(|i| i.parent.is_some()) {
            return err(StatusCode::BAD_REQUEST, "invalidRequest", "Delta is only supported on the root folder");
        }
        return delta(&mut s, &query, &id, &format!("/drives/{drive}/items/{id}/delta"));
    }
    if method == Method::GET && p.starts_with("/me/drive/items/") && p.ends_with("/subscriptions/socketIo") {
        if !s.push_enabled {
            return err(StatusCode::NOT_FOUND, "itemNotFound", "push notifications disabled");
        }
        let base = s.base_url.clone();
        return ok_json(json!({
            "id": "sub-2",
            "notificationUrl": format!("{base}/notifications?token=mock-push-token&applicationId=test"),
            "expirationDateTime": (Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
        }));
    }
    if let Some(drive) = p.strip_prefix("/_drive/") {
        let d = s.drives.iter().find(|d| d.id == drive).cloned();
        return match d {
            Some(d) => ok_json(json!({
                "id": d.id, "driveType": d.drive_type, "name": d.name,
                "webUrl": format!("https://contoso.example/{}", d.id),
            })),
            None => err(StatusCode::NOT_FOUND, "itemNotFound", "no such drive"),
        };
    }
    if method == Method::GET && p == "/sites" {
        let q = query.get("search").cloned().unwrap_or_default().to_lowercase();
        let mut seen = HashSet::new();
        let sites: Vec<Value> = s
            .drives
            .iter()
            .filter_map(|d| d.site.clone())
            .filter(|(_, name)| q.is_empty() || q == "*" || name.to_lowercase().contains(&q))
            .filter(|(id, _)| seen.insert(id.clone()))
            .map(|(id, name)| {
                json!({ "id": id, "displayName": name, "name": name,
                                      "webUrl": format!("https://contoso.example/sites/{name}") })
            })
            .collect();
        return ok_json(json!({ "value": sites }));
    }
    if method == Method::GET
        && let Some(rest) = p.strip_prefix("/sites/")
    {
        let parts: Vec<&str> = rest.split('/').collect();
        let drive_json = |d: &MockDrive| {
            json!({ "id": d.id, "name": d.name, "driveType": d.drive_type,
                    "webUrl": format!("https://contoso.example/{}", d.id) })
        };
        match parts.as_slice() {
            [site, "drives"] => {
                let v: Vec<Value> = s
                    .drives
                    .iter()
                    .filter(|d| d.site.as_ref().is_some_and(|(id, _)| id == site))
                    .map(drive_json)
                    .collect();
                return ok_json(json!({ "value": v }));
            }
            [site] => {
                return match s.drives.iter().find_map(|d| d.site.clone().filter(|(id, _)| id == site)) {
                    Some((id, name)) => ok_json(json!({ "id": id, "displayName": name, "name": name })),
                    None => err(StatusCode::NOT_FOUND, "itemNotFound", "no such site"),
                };
            }
            [_site, "lists", list, "drive"] => {
                let list = pct_decode(list).trim_matches(['{', '}']).to_lowercase();
                return match s.drives.iter().find(|d| d.list_id.to_lowercase() == list) {
                    Some(d) => ok_json(drive_json(d)),
                    None => err(StatusCode::NOT_FOUND, "itemNotFound", "no such list"),
                };
            }
            _ => {}
        }
    }

    match (method.clone(), p.as_str()) {
        (Method::GET, "/me") => ok_json(json!({
            "id": "user-1", "displayName": s.user_name, "mail": s.user_email,
            "userPrincipalName": s.user_email
        })),
        (Method::GET, "/organization") => ok_json(json!({ "value": [] })),
        (Method::GET, "/me/people") => {
            let q = query.get("$search").cloned().unwrap_or_default().trim_matches('"').to_lowercase();
            let people = [
                ("Ana Pérez", "ana@contoso.com"),
                ("Bo Lind", "bo@example.org"),
                ("Anders Berg", "anders@example.org"),
            ];
            let hits: Vec<Value> = people
                .iter()
                .filter(|(n, e)| n.to_lowercase().contains(&q) || e.contains(&q))
                .map(|(n, e)| json!({ "displayName": n, "scoredEmailAddresses": [{ "address": e }] }))
                .collect();
            ok_json(json!({ "value": hits }))
        }
        (Method::GET, "/me/drive") => {
            let used: u64 = s.items.values().filter(|i| !i.deleted).map(|i| i.content.len() as u64).sum();
            ok_json(json!({
                "id": DRIVE_ID, "driveType": s.my_drive_type, "name": "OneDrive",
                "webUrl": "https://onedrive.example/",
                "owner": { "user": { "displayName": s.user_name } },
                "quota": { "total": s.quota_total, "used": used, "remaining": s.quota_total.saturating_sub(used),
                           "deleted": 0, "state": "normal" }
            }))
        }
        (Method::GET, "/me/drive/root") => {
            let root = s.items[ROOT_ID].clone();
            ok_json(s.item_json(&root))
        }
        (Method::GET, "/me/drive/root/delta") => delta(&mut s, &query, ROOT_ID, "/me/drive/root/delta"),
        (Method::GET, "/me/drive/root/subscriptions/socketIo") => {
            if !s.push_enabled {
                return err(StatusCode::NOT_FOUND, "itemNotFound", "push notifications disabled");
            }
            let base = s.base_url.clone();
            ok_json(json!({
                "id": "sub-1",
                "notificationUrl": format!("{base}/notifications?token=mock-push-token&applicationId=test"),
                "expirationDateTime": (Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
            }))
        }
        _ => {
            if let Some(rest) = p.strip_prefix("/me/drive/items/") {
                items_route(&mut s, &method, rest, &query, &headers, body)
            } else {
                err(StatusCode::NOT_FOUND, "itemNotFound", &format!("no route {method} {path}"))
            }
        }
    }
}

/// `/drives/{d}/...` → `/me/drive/...`; a drive's `/root` becomes its root item.
fn normalize_drive_path(s: &DriveState, path: &str) -> String {
    let Some(rest) = path.strip_prefix("/drives/") else { return path.to_string() };
    let (drive, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let drive = pct_decode(drive);
    if drive.eq_ignore_ascii_case(DRIVE_ID) {
        return format!("/me/drive{tail}");
    }
    match s.drive_root(&drive) {
        Some(_) if tail.is_empty() => format!("/_drive/{drive}"),
        Some(root) => match tail.strip_prefix("/root") {
            Some(t) => format!("/me/drive/items/{root}{t}"),
            None => format!("/me/drive{tail}"),
        },
        None => path.to_string(),
    }
}

fn delta(s: &mut DriveState, query: &HashMap<String, String>, scope: &str, link_path: &str) -> Response {
    s.stats.delta_calls += 1;
    let base = s.base_url.clone();
    let token = query.get("token").cloned();
    if token.as_deref() == Some("latest") {
        return ok_json(json!({
            "value": [],
            "@odata.deltaLink": format!("{base}{link_path}?token={}", s.seq),
        }));
    }
    let since: u64 = token.as_deref().and_then(|t| t.parse().ok()).unwrap_or(0);
    if since > 0 && since < s.min_valid_token {
        return err(StatusCode::GONE, "resyncRequired", "delta token expired");
    }
    let skip: usize = query.get("skip").and_then(|v| v.parse().ok()).unwrap_or(0);
    // Snapshot the end of this round on the first page so paging is stable.
    let upto: u64 = query.get("upto").and_then(|v| v.parse().ok()).unwrap_or(s.seq);

    // Parents before children: order by depth, then sequence.
    let mut changed: Vec<&MockItem> = s
        .items
        .values()
        .filter(|i| i.seq > since && i.seq <= upto && (since > 0 || !i.deleted) && s.is_under(&i.id, scope))
        .collect();
    let depth = |i: &MockItem| {
        let mut d = 0;
        let mut cur = i.parent.clone();
        while let Some(p) = cur {
            d += 1;
            cur = s.items.get(&p).and_then(|x| x.parent.clone());
        }
        d
    };
    changed.sort_by_key(|i| (depth(i), i.seq, i.id.clone()));
    let page_size = s.delta_page_size;
    let page: Vec<Value> = changed.iter().skip(skip).take(page_size).map(|i| s.item_json(i)).collect();
    let more = changed.len() > skip + page_size;
    let mut resp = json!({ "value": page });
    if more {
        resp["@odata.nextLink"] =
            json!(format!("{base}{link_path}?token={since}&skip={}&upto={upto}", skip + page_size));
    } else {
        resp["@odata.deltaLink"] = json!(format!("{base}{link_path}?token={upto}"));
    }
    ok_json(resp)
}

fn download(s: &mut DriveState, id: &str, headers: &HeaderMap, fault: Option<FaultKind>) -> Response {
    let Some(it) = s.live(id).cloned() else {
        return err(StatusCode::NOT_FOUND, "itemNotFound", "gone");
    };
    s.stats.downloads += 1;
    let mut data = it.content;
    match fault {
        Some(FaultKind::CorruptBody) => {
            let mut v = data.to_vec();
            for b in v.iter_mut().step_by(7) {
                *b ^= 0x5a;
            }
            data = Bytes::from(v);
        }
        Some(FaultKind::TruncateBody) if data.len() > 1 => {
            // Announce the full length, then send half: the connection breaks mid-body.
            let full = data.len();
            let half = data.slice(..full / 2);
            let stream = futures_util_stream(half);
            let mut resp = Response::new(axum::body::Body::from_stream(stream));
            resp.headers_mut().insert(header::CONTENT_LENGTH, full.to_string().parse().unwrap());
            return resp;
        }
        _ => {}
    }
    s.stats.bytes_downloaded += data.len() as u64;
    if let Some(range) = headers.get(header::RANGE).and_then(|v| v.to_str().ok())
        && let Some((a, b)) = range.strip_prefix("bytes=").and_then(|r| r.split_once('-'))
        && let Ok(start) = a.parse::<usize>()
    {
        let start = start.min(data.len());
        let end = b.parse::<usize>().ok().map(|e| (e + 1).min(data.len())).unwrap_or(data.len()).max(start);
        s.stats.ranged_requests += 1;
        let mut resp = (StatusCode::PARTIAL_CONTENT, data.slice(start..end)).into_response();
        resp.headers_mut().insert(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{}", end.saturating_sub(1), data.len()).parse().unwrap(),
        );
        return resp;
    }
    (StatusCode::OK, data).into_response()
}

/// A body stream that yields `data` and then fails (simulates a dropped connection).
fn futures_util_stream(data: Bytes) -> impl futures_core::Stream<Item = Result<Bytes, std::io::Error>> + Send {
    let mut parts = vec![Ok(data), Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "dropped"))];
    parts.reverse();
    struct S(Vec<Result<Bytes, std::io::Error>>);
    impl futures_core::Stream for S {
        type Item = Result<Bytes, std::io::Error>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::task::Poll::Ready(self.0.pop())
        }
    }
    S(parts)
}

fn items_route(
    s: &mut DriveState,
    method: &Method,
    rest: &str,
    query: &HashMap<String, String>,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    // Path addressing: {parent}:/{name}:{suffix}
    if let Some(colon) = rest.find(":/") {
        let parent = &rest[..colon];
        let after = &rest[colon + 2..];
        let (name_enc, suffix) = match after.find(':') {
            Some(i) => (&after[..i], &after[i + 1..]),
            None => (after, ""),
        };
        let name = pct_decode(name_enc);
        if s.live(parent).is_none() {
            return err(StatusCode::NOT_FOUND, "itemNotFound", "parent not found");
        }
        return match (method.clone(), suffix) {
            (Method::GET, "") => match s.child(parent, &name).cloned() {
                Some(c) => ok_json(s.item_json(&c)),
                None => err(StatusCode::NOT_FOUND, "itemNotFound", "no such child"),
            },
            (Method::PUT, "/content") => {
                let conflict = query.get("@microsoft.graph.conflictBehavior").cloned().unwrap_or("replace".into());
                put_new_file(s, parent, &name, body, &conflict, None)
            }
            (Method::POST, "/createUploadSession") => {
                let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
                let conflict =
                    req["item"]["@microsoft.graph.conflictBehavior"].as_str().unwrap_or("replace").to_string();
                let mtime = req["item"]["fileSystemInfo"]["lastModifiedDateTime"]
                    .as_str()
                    .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                    .map(|t| t.with_timezone(&Utc));
                if conflict == "fail" && s.child(parent, &name).is_some() {
                    return err(StatusCode::CONFLICT, "nameAlreadyExists", "exists");
                }
                new_session(s, Some(parent.into()), Some(name), None, conflict, mtime)
            }
            _ => err(StatusCode::NOT_FOUND, "itemNotFound", "bad path route"),
        };
    }

    let mut parts = rest.splitn(2, '/');
    let id = parts.next().unwrap_or("").to_string();
    let sub = parts.next().unwrap_or("");
    let Some(item) = s.live(&id).cloned() else {
        return err(StatusCode::NOT_FOUND, "itemNotFound", "item not found");
    };

    match (method.clone(), sub) {
        (Method::GET, "") => ok_json(s.item_json(&item)),
        (Method::GET, "children") => {
            let kids: Vec<Value> = s
                .items
                .values()
                .filter(|c| !c.deleted && c.parent.as_deref() == Some(id.as_str()))
                .map(|c| s.item_json(c))
                .collect();
            ok_json(json!({ "value": kids }))
        }
        (Method::POST, "children") => {
            let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let name = req["name"].as_str().unwrap_or("").to_string();
            let conflict = req["@microsoft.graph.conflictBehavior"].as_str().unwrap_or("fail");
            if name.is_empty() {
                return err(StatusCode::BAD_REQUEST, "invalidRequest", "name required");
            }
            if let Some(existing) = s.child(&id, &name).cloned() {
                match conflict {
                    "fail" => return err(StatusCode::CONFLICT, "nameAlreadyExists", "exists"),
                    "replace" if existing.folder => {
                        return (StatusCode::OK, axum::Json(s.item_json(&existing))).into_response();
                    }
                    _ => {}
                }
            }
            let new = s.create_folder(&id, &name);
            let it = s.items[&new].clone();
            (StatusCode::CREATED, axum::Json(s.item_json(&it))).into_response()
        }
        (Method::GET, "content") => {
            let base = s.base_url.clone();
            let mut resp = StatusCode::FOUND.into_response();
            resp.headers_mut().insert(header::LOCATION, format!("{base}/_download/{id}").parse().unwrap());
            resp
        }
        (Method::PUT, "content") => {
            if !if_match_ok(headers, &item) {
                return err(StatusCode::PRECONDITION_FAILED, "resourceModified", "etag mismatch");
            }
            s.replace_content(&id, body, None);
            s.stats.uploads_completed += 1;
            let it = s.items[&id].clone();
            ok_json(s.item_json(&it))
        }
        (Method::POST, "createUploadSession") => {
            if !if_match_ok(headers, &item) {
                return err(StatusCode::PRECONDITION_FAILED, "resourceModified", "etag mismatch");
            }
            let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let mtime = req["item"]["fileSystemInfo"]["lastModifiedDateTime"]
                .as_str()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&Utc));
            new_session(s, None, None, Some(id), "replace".into(), mtime)
        }
        (Method::PATCH, "") => {
            if !if_match_ok(headers, &item) {
                return err(StatusCode::PRECONDITION_FAILED, "resourceModified", "etag mismatch");
            }
            let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let new_name = req["name"].as_str().map(str::to_string);
            let new_parent = req["parentReference"]["id"].as_str().map(str::to_string);
            let target_parent = new_parent.clone().or(item.parent.clone()).unwrap_or_default();
            let target_name = new_name.clone().unwrap_or(item.name.clone());
            if let Some(other) = s.child(&target_parent, &target_name)
                && other.id != id
            {
                return err(StatusCode::CONFLICT, "nameAlreadyExists", "exists");
            }
            if new_name.is_some() || new_parent.is_some() {
                s.rename_move(&id, new_name, new_parent);
            }
            if let Some(m) = req["fileSystemInfo"]["lastModifiedDateTime"].as_str()
                && let Ok(t) = DateTime::parse_from_rfc3339(m)
            {
                let seq = s.bump();
                let it = s.items.get_mut(&id).unwrap();
                it.mtime = t.with_timezone(&Utc);
                it.etag_ver += 1;
                it.seq = seq;
            }
            let it = s.items[&id].clone();
            ok_json(s.item_json(&it))
        }
        (Method::DELETE, "") => {
            if !if_match_ok(headers, &item) {
                return err(StatusCode::PRECONDITION_FAILED, "resourceModified", "etag mismatch");
            }
            s.delete_recursive(&id);
            s.stats.deletes += 1;
            StatusCode::NO_CONTENT.into_response()
        }
        (Method::POST, "createLink") => {
            let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let kind = req["type"].as_str().unwrap_or("view").to_string();
            let scope = req["scope"].as_str().unwrap_or("anonymous").to_string();
            if req.get("retainInheritedPermissions") == Some(&json!(false)) {
                return err(
                    StatusCode::BAD_REQUEST,
                    "invalidRequest",
                    "test guard: never send retainInheritedPermissions=false",
                );
            }
            if scope == "anonymous" && !s.allow_anonymous_links {
                return err(
                    StatusCode::BAD_REQUEST,
                    "invalidRequest",
                    "Anyone links are disabled by your administrator",
                );
            }
            let personal = s.drive_of(&id).1 == "personal";
            if personal && (scope == "organization" || matches!(kind.as_str(), "review" | "blocksDownload")) {
                return err(StatusCode::BAD_REQUEST, "invalidRequest", "not supported for personal accounts");
            }
            // Reuse: same type and scope returns the existing link, old settings included.
            if let Some(existing) = s.permissions.get(&id).and_then(|v| {
                v.iter().find(|p| p["link"]["type"] == json!(kind) && p["link"]["scope"] == json!(scope)).cloned()
            }) {
                return ok_json(existing);
            }
            s.next_perm += 1;
            let pid = format!("link-{}", s.next_perm);
            let mut perm = json!({
                "id": pid,
                "roles": [if kind == "edit" { "write" } else { "read" }],
                "link": { "type": kind, "scope": scope, "webUrl": format!("https://1drv.example/{kind}/{id}/{pid}") },
                "hasPassword": req["password"].as_str().is_some_and(|p| !p.is_empty()),
                "expirationDateTime": req["expirationDateTime"].as_str().unwrap_or("0001-01-01T00:00:00Z"),
            });
            if let Some(r) = req["recipients"].as_array() {
                perm["grantedToIdentitiesV2"] = json!(
                    r.iter()
                        .map(|x| json!({"user": {"email": x["email"], "displayName": x["email"]}}))
                        .collect::<Vec<_>>()
                );
            }
            s.permissions.entry(id.clone()).or_default().push(perm.clone());
            let seq = s.bump();
            let it = s.items.get_mut(&id).unwrap();
            it.shared = true;
            it.seq = seq;
            (StatusCode::CREATED, axum::Json(perm)).into_response()
        }
        (Method::POST, "invite") => {
            let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            if req["requireSignIn"] == json!(false) && req["sendInvitation"] == json!(false) {
                return err(
                    StatusCode::BAD_REQUEST,
                    "invalidRequest",
                    "RequireSignIn and SendInvitation cannot both be false",
                );
            }
            let role = req["roles"][0].as_str().unwrap_or("read").to_string();
            let mut out = Vec::new();
            let mut any_failed = false;
            for r in req["recipients"].as_array().cloned().unwrap_or_default() {
                let email = r["email"].as_str().unwrap_or("").to_string();
                if email.contains("invalid") || !email.contains('@') {
                    any_failed = true;
                    out.push(json!({ "invitation": {"email": email}, "error": {
                        "code": "invalidRequest", "message": "The recipient is invalid",
                        "localizedMessage": format!("{email} isn't a valid recipient"), "fixItUrl": "https://example/fix" } }));
                    continue;
                }
                s.next_perm += 1;
                let pid = format!("grant-{}", s.next_perm);
                let name = email.split('@').next().unwrap_or("").to_string();
                let perm = json!({
                    "id": pid, "roles": [role],
                    "grantedTo": { "user": { "id": format!("u-{name}"), "displayName": name } },
                    "invitation": { "email": email, "signInRequired": true },
                    "link": { "webUrl": format!("https://1drv.example/t/{pid}") },
                    "expirationDateTime": req["expirationDateTime"].as_str().unwrap_or("0001-01-01T00:00:00Z"),
                });
                s.permissions.entry(id.clone()).or_default().push(perm.clone());
                out.push(perm);
            }
            let seq = s.bump();
            let it = s.items.get_mut(&id).unwrap();
            it.shared = true;
            it.seq = seq;
            let status = if any_failed { StatusCode::MULTI_STATUS } else { StatusCode::OK };
            (status, axum::Json(json!({ "value": out }))).into_response()
        }
        (Method::GET, "permissions") => {
            let mut v = vec![json!({ "id": "owner", "roles": ["owner"],
                "grantedTo": { "user": { "id": "user-1", "displayName": s.user_name, "email": s.user_email } } })];
            v.extend(s.permissions.get(&id).cloned().unwrap_or_default());
            // Personal drives report permissions inherited from parent folders.
            let mut cur = item.parent.clone();
            while let Some(pid) = cur {
                if let Some(ps) = s.permissions.get(&pid) {
                    let path = s.path_of(&pid);
                    for p in ps {
                        let mut p = p.clone();
                        p["inheritedFrom"] =
                            json!({ "driveId": DRIVE_ID, "id": pid, "path": format!("/drive/root:{path}") });
                        v.push(p);
                    }
                }
                cur = s.items.get(&pid).and_then(|i| i.parent.clone());
            }
            ok_json(json!({ "value": v }))
        }
        (m, sub) if sub.starts_with("permissions/") => {
            let mut parts = sub.split('/');
            let _ = parts.next();
            let pid = parts.next().unwrap_or("").to_string();
            let action = parts.next().unwrap_or("");
            if pid == "owner" {
                return err(StatusCode::FORBIDDEN, "accessDenied", "the owner can't be removed");
            }
            let Some(list) = s.permissions.get_mut(&id) else {
                return err(StatusCode::BAD_REQUEST, "invalidRequest", "inherited permissions can't be changed here");
            };
            let Some(pos) = list.iter().position(|p| p["id"] == json!(pid)) else {
                return err(StatusCode::BAD_REQUEST, "invalidRequest", "inherited permissions can't be changed here");
            };
            match (m, action) {
                (Method::DELETE, "") => {
                    list.remove(pos);
                    let empty = list.is_empty();
                    let seq = s.bump();
                    let it = s.items.get_mut(&id).unwrap();
                    it.shared = !empty;
                    it.seq = seq;
                    StatusCode::NO_CONTENT.into_response()
                }
                (Method::PATCH, "") => {
                    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
                    if list[pos]["link"]["type"].is_string() {
                        return err(
                            StatusCode::BAD_REQUEST,
                            "invalidRequest",
                            "Invalid input: No Information provided to update the specifed permission",
                        );
                    }
                    list[pos]["roles"] = req["roles"].clone();
                    ok_json(list[pos].clone())
                }
                (Method::POST, "revokeGrants") => {
                    if list[pos]["link"]["scope"] != json!("users") {
                        return err(
                            StatusCode::BAD_REQUEST,
                            "invalidRequest",
                            "revokeGrants only works on specific-people links",
                        );
                    }
                    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
                    let gone: Vec<String> = req["grantees"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|g| g["email"].as_str().map(str::to_string))
                        .collect();
                    if let Some(ids) = list[pos]["grantedToIdentitiesV2"].as_array_mut() {
                        ids.retain(|x| !gone.iter().any(|g| x["user"]["email"] == json!(g)));
                    }
                    ok_json(list[pos].clone())
                }
                _ => err(StatusCode::NOT_FOUND, "itemNotFound", "bad permission route"),
            }
        }
        (Method::GET, "versions") => {
            let mut v: Vec<Value> = item
                .versions
                .iter()
                .map(|(vid, data, t)| json!({ "id": vid, "lastModifiedDateTime": t.to_rfc3339(), "size": data.len() }))
                .collect();
            v.push(
                json!({ "id": "current", "lastModifiedDateTime": item.mtime.to_rfc3339(), "size": item.content.len(),
                           "lastModifiedBy": { "user": { "displayName": s.user_name } } }),
            );
            v.reverse();
            ok_json(json!({ "value": v }))
        }
        (Method::GET, "thumbnails") => {
            // Images and videos get a preview; other files (e.g. PDFs here) have none.
            let lower = item.name.to_lowercase();
            if [".jpg", ".jpeg", ".png", ".mp4", ".mkv"].iter().any(|e| lower.ends_with(e)) {
                let base = s.base_url.clone();
                ok_json(json!({ "value": [{
                    "id": "0",
                    "large": { "url": format!("{base}/_thumb/{id}"), "width": 800, "height": 600 },
                    "medium": { "url": format!("{base}/_thumb/{id}"), "width": 176, "height": 132 }
                }] }))
            } else {
                ok_json(json!({ "value": [] }))
            }
        }
        (m, sub) if sub.starts_with("versions/") => {
            let mut p = sub.split('/');
            let _ = p.next();
            let vid = p.next().unwrap_or("");
            let action = p.next().unwrap_or("");
            let Some((_, data, _)) = item.versions.iter().find(|(v, _, _)| v == vid).cloned() else {
                return err(StatusCode::NOT_FOUND, "itemNotFound", "no version");
            };
            match (m, action) {
                (Method::POST, "restoreVersion") => {
                    s.replace_content(&id, data, None);
                    StatusCode::NO_CONTENT.into_response()
                }
                (Method::GET, "content") => (StatusCode::OK, data).into_response(),
                _ => err(StatusCode::NOT_FOUND, "itemNotFound", "bad version route"),
            }
        }
        _ => err(StatusCode::NOT_FOUND, "itemNotFound", &format!("no route for {method} {sub}")),
    }
}

fn put_new_file(
    s: &mut DriveState,
    parent: &str,
    name: &str,
    body: Bytes,
    conflict: &str,
    mtime: Option<DateTime<Utc>>,
) -> Response {
    let existing = s.child(parent, name).cloned();
    let id = match (existing, conflict) {
        (Some(_), "fail") => return err(StatusCode::CONFLICT, "nameAlreadyExists", "exists"),
        (Some(e), "replace") if !e.folder => {
            s.replace_content(&e.id, body, mtime);
            e.id
        }
        (Some(_), _) => {
            let mut n = 1;
            let (stem, ext) = od_core::names::split_ext(name);
            let mut candidate;
            loop {
                candidate = format!("{stem} {n}{ext}");
                if s.child(parent, &candidate).is_none() {
                    break;
                }
                n += 1;
            }
            s.create_file(parent, &candidate, body, mtime)
        }
        (None, _) => s.create_file(parent, name, body, mtime),
    };
    s.stats.uploads_completed += 1;
    let it = s.items[&id].clone();
    (StatusCode::CREATED, axum::Json(s.item_json(&it))).into_response()
}

fn new_session(
    s: &mut DriveState,
    parent: Option<String>,
    name: Option<String>,
    item: Option<String>,
    conflict: String,
    mtime: Option<DateTime<Utc>>,
) -> Response {
    let token = format!("sess{}", s.bump());
    s.sessions.insert(
        token.clone(),
        UploadSession {
            target_parent: parent,
            target_name: name,
            target_item: item,
            conflict,
            received: vec![],
            total: None,
            mtime,
        },
    );
    let base = s.base_url.clone();
    ok_json(json!({
        "uploadUrl": format!("{base}/_upload/{token}"),
        "expirationDateTime": (Utc::now() + chrono::Duration::days(1)).to_rfc3339(),
        "nextExpectedRanges": ["0-"],
    }))
}

fn upload_session_request(
    s: &mut DriveState,
    method: &Method,
    token: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    if headers.contains_key(header::AUTHORIZATION) {
        return err(StatusCode::UNAUTHORIZED, "unauthenticated", "uploadUrl must not carry a token");
    }
    let fault = s.take_fault(method, &format!("/_upload/{token}"));
    if let Some(f) = fault.as_ref().filter(|f| f.kind != FaultKind::AfterCommit) {
        return err(StatusCode::from_u16(f.status).unwrap(), &f.code, "injected fault");
    }
    let resp = upload_session_inner(s, method, token, headers, body);
    match fault {
        Some(f) => err(StatusCode::from_u16(f.status).unwrap(), &f.code, "injected fault after commit"),
        None => resp,
    }
}

fn upload_session_inner(
    s: &mut DriveState,
    method: &Method,
    token: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    if *method == Method::DELETE {
        s.sessions.remove(token);
        return StatusCode::NO_CONTENT.into_response();
    }
    let Some(sess) = s.sessions.get_mut(token) else {
        return err(StatusCode::NOT_FOUND, "itemNotFound", "session expired");
    };
    if *method == Method::GET {
        return ok_json(json!({ "nextExpectedRanges": [format!("{}-", sess.received.len())] }));
    }
    let range = headers.get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()).unwrap_or("");
    // "bytes 0-99/1000"
    let parsed = range.strip_prefix("bytes ").and_then(|r| {
        let (span, total) = r.split_once('/')?;
        let (a, b) = span.split_once('-')?;
        Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?, total.parse::<u64>().ok()?))
    });
    let Some((start, end, total)) = parsed else {
        return err(StatusCode::BAD_REQUEST, "invalidRange", "bad Content-Range");
    };
    if start != sess.received.len() as u64 || end + 1 - start != body.len() as u64 {
        return err(StatusCode::RANGE_NOT_SATISFIABLE, "invalidRange", "unexpected range");
    }
    sess.received.extend_from_slice(&body);
    sess.total = Some(total);
    s.stats.bytes_uploaded += body.len() as u64;
    let sess = s.sessions.get_mut(token).unwrap();
    if (sess.received.len() as u64) < total {
        return (
            StatusCode::ACCEPTED,
            axum::Json(json!({ "nextExpectedRanges": [format!("{}-", sess.received.len())] })),
        )
            .into_response();
    }
    let sess = s.sessions.remove(token).unwrap();
    let data = Bytes::from(sess.received);
    if let Some(item) = sess.target_item {
        if s.live(&item).is_none() {
            return err(StatusCode::NOT_FOUND, "itemNotFound", "target vanished");
        }
        s.replace_content(&item, data, sess.mtime);
        s.stats.uploads_completed += 1;
        let it = s.items[&item].clone();
        return ok_json(s.item_json(&it));
    }
    let parent = sess.target_parent.unwrap_or_default();
    let name = sess.target_name.unwrap_or_default();
    put_new_file(s, &parent, &name, data, &sess.conflict, sess.mtime)
}

// ------------------------------------------------------------------ socket.io (Engine.IO v4)

async fn socket_io(
    State(state): State<Arc<Mutex<DriveState>>>,
    ws: axum::extract::WebSocketUpgrade,
    uri: Uri,
) -> Response {
    let q = query_map(&uri);
    if q.get("EIO").map(String::as_str) != Some("4")
        || q.get("transport").map(String::as_str) != Some("websocket")
        || q.get("token").map(String::as_str) != Some("mock-push-token")
    {
        return err(StatusCode::BAD_REQUEST, "invalidRequest", "bad socket.io request");
    }
    let rx = state.lock().unwrap().changes.subscribe();
    ws.on_upgrade(move |socket| serve_socket(socket, rx))
}

async fn serve_socket(mut socket: axum::extract::ws::WebSocket, mut rx: tokio::sync::watch::Receiver<u64>) {
    use axum::extract::ws::Message;
    let open = r#"0{"sid":"mock-sid","upgrades":[],"pingInterval":2000,"pingTimeout":2000,"maxPayload":1000000}"#;
    if socket.send(Message::Text(open.into())).await.is_err() {
        return;
    }
    let mut ping = tokio::time::interval(std::time::Duration::from_secs(2));
    let mut joined = false;
    loop {
        tokio::select! {
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(t))) if t.as_str() == "40/notifications," => {
                    joined = true;
                    let _ = socket.send(Message::Text(r#"40/notifications,{"sid":"ns-sid"}"#.into())).await;
                }
                Some(Ok(_)) => {}
                _ => return,
            },
            changed = rx.changed() => {
                if changed.is_err() {
                    return;
                }
                if joined {
                    let event = r#"42/notifications,["notification","{\"value\":[{\"subscriptionId\":\"sub-1\"}]}"]"#;
                    if socket.send(Message::Text(event.into())).await.is_err() {
                        return;
                    }
                }
            }
            _ = ping.tick() => {
                if socket.send(Message::Text("2".into())).await.is_err() {
                    return;
                }
            }
        }
    }
}
