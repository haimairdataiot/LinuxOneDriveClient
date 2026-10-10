//! Sharing (Share dialog, Copy link, Manage access) through the real daemon.

mod common;

use od_core::dbus::{AccessEntry, InviteOutcome, LinkResult, PathInfo, Person, SharingInfo, StopSharingOutcome};
use od_mockgraph::MockGraph;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn share_copy_link_invite_manage_and_stop() {
    let mock = MockGraph::start().await;
    mock.put_file("/Docs/plan.docx", b"the plan");
    let s = common::start(mock, |_| {}).await;
    let file = s.mount.join("Docs/plan.docx");
    let f2 = file.clone();
    common::wait_until("file visible", 15, || {
        let f = f2.clone();
        async move { tokio::task::spawn_blocking(move || f.exists()).await.unwrap() }
    })
    .await;
    let path = file.to_str().unwrap();

    let info: SharingInfo = serde_json::from_str(&s.acct.get_sharing_info(path).await.unwrap()).unwrap();
    assert!(info.can_share);
    assert_eq!((info.default_audience.as_str(), info.default_role.as_str()), ("anyone", "edit"));
    let root: SharingInfo =
        serde_json::from_str(&s.acct.get_sharing_info(s.mount.to_str().unwrap()).await.unwrap()).unwrap();
    assert!(!root.can_share, "the whole OneDrive can't be shared");

    // Copy link: default link, reused on the second call.
    let url1 = s.acct.copy_link(path).await.unwrap();
    assert_eq!(s.acct.copy_link(path).await.unwrap(), url1);

    // New settings on the same kind of link: OneDrive hands back the existing
    // link; it's only replaced when asked (its holders would lose access).
    let expiry = chrono::Utc::now().timestamp() + 7 * 86_400;
    let opts = format!(r#"{{"audience":"anyone","role":"edit","expires":{expiry}}}"#);
    let r: LinkResult = serde_json::from_str(&s.acct.create_link(path, &opts).await.unwrap()).unwrap();
    assert!(r.existing && !r.created && !r.settings_applied);
    assert_eq!((r.url.as_str(), r.expires), (url1.as_str(), 0));
    let opts = format!(r#"{{"audience":"anyone","role":"edit","expires":{expiry},"replace":true}}"#);
    let r: LinkResult = serde_json::from_str(&s.acct.create_link(path, &opts).await.unwrap()).unwrap();
    assert!(r.settings_applied && r.created && !r.existing);
    assert_ne!(r.url, url1);
    assert!(r.expires > 0);
    // "Copy link" (no settings) now copies that link as it is, expiry included.
    assert_eq!(s.acct.copy_link(path).await.unwrap(), r.url);

    // Invite with a partial failure.
    let out: InviteOutcome = serde_json::from_str(
        &s.acct
            .invite(path, r#"{"recipients":["ana@contoso.com","invalid-x"],"role":"edit","message":"Have a look"}"#)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(out.granted, 1);
    assert_eq!(out.failed.len(), 1);

    // Manage access.
    let list: Vec<AccessEntry> = serde_json::from_str(&s.acct.list_access(path).await.unwrap()).unwrap();
    assert!(list.iter().any(|e| e.kind == "owner"));
    let link = list.iter().find(|e| e.kind == "link").unwrap();
    assert!(link.expires > 0);
    let person = list.iter().find(|e| e.kind == "person").unwrap();
    assert_eq!(person.people[0].email, "ana@contoso.com");
    assert!(person.role_editable);
    s.acct.set_access_role(path, &person.id, "view").await.unwrap();
    s.acct.remove_access(path, &link.id).await.unwrap();
    let list: Vec<AccessEntry> = serde_json::from_str(&s.acct.list_access(path).await.unwrap()).unwrap();
    assert!(!list.iter().any(|e| e.kind == "link"));
    assert_eq!(list.iter().find(|e| e.kind == "person").unwrap().role, "view");

    // Shared emblem.
    let info: Vec<PathInfo> = serde_json::from_str(&s.mgr.paths_info(&[path]).await.unwrap()).unwrap();
    assert!(info[0].shared);

    // Suggestions: recent recipients first; Microsoft 365 people after consent.
    let people: Vec<Person> = serde_json::from_str(&s.acct.search_people("an").await.unwrap()).unwrap();
    assert_eq!(people.len(), 1);
    assert_eq!(people[0].email, "ana@contoso.com");
    let (url, redirect) = s.acct.begin_people_consent().await.unwrap();
    assert!(url.contains("People.Read"));
    let state = url::Url::parse(&url).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();
    s.mgr.complete_sign_in(&format!("{redirect}?code=c&state={state}")).await.unwrap();
    let people: Vec<Person> = serde_json::from_str(&s.acct.search_people("an").await.unwrap()).unwrap();
    assert!(people.iter().any(|p| p.name == "Anders Berg"), "{people:?}");
    let info: SharingInfo = serde_json::from_str(&s.acct.get_sharing_info(path).await.unwrap()).unwrap();
    assert!(info.people_suggestions);

    // Stop sharing removes everything but the owner.
    let out: StopSharingOutcome = serde_json::from_str(&s.acct.stop_sharing(path).await.unwrap()).unwrap();
    assert!(out.removed >= 1 && out.failed.is_empty());
    let list: Vec<AccessEntry> = serde_json::from_str(&s.acct.list_access(path).await.unwrap()).unwrap();
    assert_eq!(list.len(), 1);
    let info: Vec<PathInfo> = serde_json::from_str(&s.mgr.paths_info(&[path]).await.unwrap()).unwrap();
    assert!(!info[0].shared);
}
