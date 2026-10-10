//! Graph client against the in-memory mock server.

use bytes::Bytes;
use od_graph::client::{FragmentResult, UPLOAD_FRAGMENT, build_http_client};
use od_graph::{ConflictBehavior, GraphClient, GraphError, ItemPatch, TokenManager, UploadTarget};
use od_mockgraph::{MockGraph, ROOT_ID, VALID_TOKEN};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

async fn setup() -> (MockGraph, GraphClient) {
    let mock = MockGraph::start().await;
    let http = build_http_client();
    let tokens = Arc::new(TokenManager::fixed(http.clone(), VALID_TOKEN));
    let client = GraphClient::new(http, mock.base_url(), tokens).with_max_attempts(4);
    (mock, client)
}

#[tokio::test]
async fn identity_and_drive() {
    let (_mock, g) = setup().await;
    let me = g.me().await.unwrap();
    assert_eq!(me.email(), "mock.user@example.com");
    let drive = g.drive().await.unwrap();
    assert_eq!(drive.drive_type.as_deref(), Some("personal"));
    assert!(drive.quota.unwrap().total.unwrap() > 0);
    assert!(g.root().await.unwrap().is_root());
}

#[tokio::test]
async fn folders_uploads_downloads_and_mutations() {
    let (mock, g) = setup().await;
    let docs = g.create_folder(ROOT_ID, "Docs", ConflictBehavior::Fail).await.unwrap();
    let dup = g.create_folder(ROOT_ID, "docs", ConflictBehavior::Fail).await.unwrap_err();
    assert!(dup.is_name_conflict(), "{dup}");

    // Empty file via simple upload.
    let empty = g
        .upload_small(
            &UploadTarget::New { parent_id: docs.id.clone(), name: "empty.txt".into() },
            Bytes::new(),
            ConflictBehavior::Replace,
        )
        .await
        .unwrap();
    assert_eq!(empty.size, Some(0));

    // Multi-fragment session upload (2.5 fragments) with a modification time.
    let data: Vec<u8> = (0..(UPLOAD_FRAGMENT * 5 / 2)).map(|i| (i % 251) as u8).collect();
    let mtime = chrono::DateTime::parse_from_rfc3339("2024-05-06T07:08:09Z").unwrap().to_utc();
    let target = UploadTarget::New { parent_id: docs.id.clone(), name: "big file #1.bin".into() };
    let session = g.create_upload_session(&target, Some(mtime), None, ConflictBehavior::Replace).await.unwrap();
    let mut offset = 0u64;
    let item = loop {
        let end = (offset as usize + UPLOAD_FRAGMENT).min(data.len());
        let chunk = Bytes::copy_from_slice(&data[offset as usize..end]);
        match g.upload_fragment(&session.upload_url, chunk, offset, data.len() as u64).await.unwrap() {
            FragmentResult::Continue(next) => offset = next,
            FragmentResult::Complete(item) => break item,
        }
    };
    assert_eq!(item.size, Some(data.len() as i64));
    assert_eq!(item.modified(), Some(mtime));
    assert_eq!(mock.read("/Docs/big file #1.bin").unwrap(), data);

    // Download with hash verification, then a resumed download.
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("dl.bin");
    let cancel = CancellationToken::new();
    let hash = g
        .download_to(&item.id, &dest, Some(data.len() as u64), item.quick_xor_hash(), false, &cancel, &|_| {})
        .await
        .unwrap();
    assert_eq!(Some(hash.as_str()), item.quick_xor_hash());
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    std::fs::write(&dest, &data[..1000]).unwrap();
    g.download_to(&item.id, &dest, Some(data.len() as u64), item.quick_xor_hash(), true, &cancel, &|_| {})
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);

    // A wrong expected hash is detected and the partial file discarded.
    let bad = g
        .download_to(&item.id, &dest, None, Some("AAAAAAAAAAAAAAAAAAAAAAAAAAA="), false, &cancel, &|_| {})
        .await
        .unwrap_err();
    assert!(matches!(bad, GraphError::HashMismatch));
    assert_eq!(std::fs::metadata(&dest).unwrap().len(), 0);

    // Rename + move, eTag precondition, delete.
    let archive = g.create_folder(ROOT_ID, "Archive", ConflictBehavior::Fail).await.unwrap();
    let stale_etag = item.e_tag.clone().unwrap();
    let moved = g
        .update_item(
            &item.id,
            &ItemPatch { name: Some("renamed.bin".into()), parent_id: Some(archive.id.clone()), ..Default::default() },
            Some(&stale_etag),
        )
        .await
        .unwrap();
    assert!(mock.exists("/Archive/renamed.bin"));
    let precond = g.delete_item(&moved.id, Some(&stale_etag)).await.unwrap_err();
    assert!(precond.is_precondition_failed(), "{precond}");
    g.delete_item(&moved.id, moved.e_tag.as_deref()).await.unwrap();
    assert!(!mock.exists("/Archive/renamed.bin"));
    // Deleting again is idempotent.
    g.delete_item(&moved.id, None).await.unwrap();

    let link = g.create_link(&empty.id, "view", "anonymous").await.unwrap();
    assert!(link.starts_with("https://"));
}

#[tokio::test]
async fn delta_paging_changes_and_resync() {
    let (mock, g) = setup().await;
    mock.set_delta_page_size(3);
    for i in 0..7 {
        mock.put_file(&format!("/A/f{i}.txt"), b"x");
    }
    // Initial enumeration over several pages.
    let mut link: Option<String> = None;
    let mut seen = Vec::new();
    let delta_link = loop {
        let page = g.delta_page(link.as_deref()).await.unwrap();
        seen.extend(page.value.into_iter().map(|i| i.name.unwrap_or_default()));
        match (page.next_link, page.delta_link) {
            (Some(n), _) => link = Some(n),
            (None, Some(d)) => break d,
            _ => panic!("no link"),
        }
    };
    assert_eq!(seen.len(), 9, "{seen:?}"); // root + A + 7 files

    mock.put_file("/A/f1.txt", b"changed");
    mock.remove("/A/f2.txt");
    let mut names: Vec<(String, bool)> = Vec::new();
    let mut link = delta_link.clone();
    let next_delta = loop {
        let page = g.delta_page(Some(&link)).await.unwrap();
        names.extend(page.value.iter().map(|i| (i.name.clone().unwrap_or_default(), i.is_deleted())));
        match (page.next_link, page.delta_link) {
            (Some(n), _) => link = n,
            (None, Some(d)) => break d,
            _ => panic!("no link"),
        }
    };
    assert!(names.contains(&("f1.txt".into(), false)), "{names:?}");
    assert!(names.contains(&("f2.txt".into(), true)), "{names:?}");

    mock.expire_delta_tokens();
    let err = g.delta_page(Some(&next_delta)).await.unwrap_err();
    assert!(matches!(err, GraphError::ResyncRequired { .. }), "{err}");
}

#[tokio::test]
async fn retries_throttling_and_server_errors() {
    let (mock, g) = setup().await;
    mock.fail("/me/drive", 429, 2);
    let started = std::time::Instant::now();
    g.drive().await.unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_secs(2), "Retry-After honoured");

    mock.fail("/me/drive/root", 503, 1);
    g.root().await.unwrap();

    mock.fail("/me/drive/items", 400, 1);
    let e = g.item(ROOT_ID).await.unwrap_err();
    assert_eq!(e.status(), Some(400));
}

#[tokio::test]
async fn sharing_links_invites_and_access() {
    use od_graph::sharing::LinkRequest;
    let (mock, g) = setup().await;
    let folder = mock.mkdir("/Team");
    mock.put_file("/Team/plan.docx", b"plan");
    let file = mock.id_of("/Team/plan.docx").unwrap();

    // Links: created once, then reused (200) with the same id.
    let req = LinkRequest { kind: "view".into(), scope: "anonymous".into(), ..Default::default() };
    let (p1, created) = g.create_link_with(&file, &req).await.unwrap();
    assert!(created);
    let (p2, created2) = g.create_link_with(&file, &req).await.unwrap();
    assert!(!created2);
    assert_eq!(p1.id, p2.id);
    // Not available on personal accounts → a clean API error.
    let org = LinkRequest { kind: "view".into(), scope: "organization".into(), ..Default::default() };
    assert_eq!(g.create_link_with(&file, &org).await.unwrap_err().status(), Some(400));

    // Invitations: partial failure is reported per recipient.
    let r = g
        .invite(&file, &["ana@contoso.com".into(), "invalid-person".into()], "write", "Please review", true, None, None)
        .await
        .unwrap();
    assert_eq!(r.granted.len(), 1);
    assert_eq!(r.failed.len(), 1);
    assert_eq!(r.failed[0].0, "invalid-person");

    // Manage access: owner + link + person.
    let perms = g.list_permissions(&file).await.unwrap();
    assert!(perms.iter().any(|p| p.roles == ["owner"]));
    let grant = perms.iter().find(|p| p.invitation.is_some()).unwrap();
    assert_eq!(grant.people()[0].1, "ana@contoso.com");
    // Direct grants can change role; links can't (API limitation).
    g.update_permission_roles(&file, &grant.id, "read").await.unwrap();
    assert!(g.update_permission_roles(&file, &p1.id, "write").await.is_err());
    // Remove the link; the owner can't be removed.
    g.delete_permission(&file, &p1.id).await.unwrap();
    assert!(g.delete_permission(&file, "owner").await.is_err());

    // Folder permissions show up as inherited on the file.
    g.create_link_with(&folder, &LinkRequest { kind: "edit".into(), scope: "anonymous".into(), ..Default::default() })
        .await
        .unwrap();
    let perms = g.list_permissions(&file).await.unwrap();
    assert!(perms.iter().any(|p| p.inherited_from.is_some()));

    let hits = g.search_people("an").await.unwrap();
    assert!(hits.iter().any(|h| h.email == "ana@contoso.com"));
    assert!(g.sharing_state(&file).await.unwrap().0, "item is marked shared");
}
