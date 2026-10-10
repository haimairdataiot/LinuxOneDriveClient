//! linux-onedrivectl: command-line control of the Linux OneDrive daemon.

use anyhow::{Context, anyhow, bail};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use od_core::dbus::{
    AccessEntry, AccountProxy, ActivityEntry, InviteOptions, InviteOutcome, Issue, LibraryInfo, LinkOptions,
    LinkResult, ManagerProxy, PathInfo, Progress, SharingInfo, SiteInfo, StopSharingOutcome, SyncLocation, VersionInfo,
};
use std::path::PathBuf;
use zbus::proxy::CacheProperties;

#[derive(Parser)]
#[command(name = "linux-onedrivectl", version, about = "Control Linux OneDrive from the command line")]
struct Cli {
    /// Account to act on (id or email); defaults to the first account.
    #[arg(short, long, global = true)]
    account: Option<String>,
    /// Machine-readable JSON output where supported.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show sync status for every account.
    Status,
    /// List linked accounts.
    Accounts,
    /// Sign in to a Microsoft account (browser by default).
    Login {
        /// Email address to pre-fill.
        email: Option<String>,
        /// Use a code shown here instead of a browser (work/school accounts).
        #[arg(long)]
        device: bool,
        /// Where to put the OneDrive folder for a new account.
        #[arg(long)]
        folder: Option<PathBuf>,
    },
    /// Unlink an account from this device.
    Logout,
    /// Pause syncing (e.g. 2h, 8h, 24h, 30m) or indefinitely without a duration.
    Pause { duration: Option<String> },
    /// Resume syncing.
    Resume,
    /// Always keep files/folders on this device.
    Pin { paths: Vec<PathBuf> },
    /// Stop always keeping files on this device (they stay until space is freed).
    Unpin { paths: Vec<PathBuf> },
    /// Free up space: make files online-only.
    Free { paths: Vec<PathBuf> },
    /// Status of files (online-only, local, pinned, syncing, error).
    Info { paths: Vec<PathBuf> },
    /// Create a sharing link and print it (default: your "Default link" setting).
    Share {
        path: PathBuf,
        /// Allow editing.
        #[arg(long, conflicts_with = "view")]
        edit: bool,
        /// View only.
        #[arg(long)]
        view: bool,
        /// Only people in your organization (work or school accounts).
        #[arg(long, conflicts_with = "people")]
        org: bool,
        /// Only these people (comma-separated email addresses).
        #[arg(long, value_delimiter = ',')]
        people: Vec<String>,
        /// Stop working after this many days.
        #[arg(long)]
        expires: Option<u32>,
        /// Require a password to open the link.
        #[arg(long)]
        password: Option<String>,
        /// If a link of this kind already exists with other settings, replace
        /// it (people who have the current link lose access).
        #[arg(long)]
        replace: bool,
    },
    /// Give people access by email (sends them an invitation).
    Invite {
        path: PathBuf,
        /// Email addresses.
        #[arg(required = true, value_delimiter = ',')]
        emails: Vec<String>,
        /// Allow editing (default: view only).
        #[arg(long)]
        edit: bool,
        /// Message included in the invitation.
        #[arg(long, short)]
        message: Option<String>,
        /// Don't send an email.
        #[arg(long)]
        no_email: bool,
    },
    /// Show who has access to a file or folder (links and people).
    Access { path: PathBuf },
    /// Remove one entry from `access` by its id, or all sharing with --all.
    Unshare {
        path: PathBuf,
        /// Id shown by `access`.
        id: Option<String>,
        /// Stop sharing completely: remove every link and person.
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
    /// Print the web address of a file or folder.
    Link { path: PathBuf },
    /// List previous versions of a file.
    Versions { path: PathBuf },
    /// Restore a previous version of a file.
    RestoreVersion { path: PathBuf, version: String },
    /// Shared folders (shortcuts) and SharePoint libraries synced with an account.
    Locations,
    /// Stop syncing a library, or remove a shortcut (key from `locations`).
    RemoveLocation { key: String },
    /// Find SharePoint sites (work or school accounts).
    Sites { query: Option<String> },
    /// Document libraries of a site.
    Libraries { site_id: String },
    /// Start syncing a document library.
    AddLibrary { site_id: String, drive_id: String },
    /// Open an odopen:// link (a library's "Sync" button in the browser).
    OpenLink { url: String },
    /// Check for a new version of Linux OneDrive, and optionally install it.
    Update {
        /// Download the new version (verified against the release's checksums)
        /// and install it. Asks for your administrator password.
        #[arg(long)]
        install: bool,
    },
    /// Show recent activity.
    Activity {
        #[arg(short, default_value_t = 20)]
        n: u32,
    },
    /// Show files that can't sync.
    Issues {
        /// Retry everything that failed.
        #[arg(long)]
        retry: bool,
    },
    /// Answer a held mass delete.
    MassDelete {
        /// Delete the files online too.
        #[arg(long, conflicts_with = "restore")]
        delete: bool,
        /// Restore the files on this device.
        #[arg(long)]
        restore: bool,
    },
    /// Check for remote changes now.
    Refresh,
    /// Back up Desktop/Documents/Pictures/Music/Videos to OneDrive: `backup`, `backup start documents`, `backup stop documents`.
    Backup { action: Option<String>, folder: Option<String> },
    /// List files deleted online whose downloaded copies are still kept.
    Removed,
    /// Put a kept copy of a removed file back (see `removed`).
    RestoreRemoved { id: String },
    /// Show or change settings: `settings`, `settings general.poll_interval_secs=120`.
    Settings { assignments: Vec<String> },
    /// Stop the sync daemon (unmounts OneDrive folders).
    Quit,
}

async fn conn() -> anyhow::Result<zbus::Connection> {
    zbus::Connection::session().await.context("connecting to the session bus")
}

async fn manager(c: &zbus::Connection) -> anyhow::Result<ManagerProxy<'static>> {
    Ok(ManagerProxy::builder(c).cache_properties(CacheProperties::No).build().await?)
}

async fn account_proxy(c: &zbus::Connection, path: &str) -> anyhow::Result<AccountProxy<'static>> {
    Ok(AccountProxy::builder(c).path(path.to_string())?.cache_properties(CacheProperties::No).build().await?)
}

async fn pick_account(
    c: &zbus::Connection,
    m: &ManagerProxy<'_>,
    want: Option<&str>,
) -> anyhow::Result<AccountProxy<'static>> {
    let paths = m.accounts().await?;
    if paths.is_empty() {
        bail!("no account is linked; run `linux-onedrivectl login`");
    }
    for p in &paths {
        let a = account_proxy(c, p.as_str()).await?;
        match want {
            None => return Ok(a),
            Some(w) if a.id().await? == w || a.email().await?.eq_ignore_ascii_case(w) => return Ok(a),
            _ => {}
        }
    }
    bail!("no account matches {:?}", want.unwrap_or_default())
}

/// Find the account owning a path.
/// Absolute form of a path as the user wrote it: `.` and `..` are resolved
/// by name, links are not followed (a shared folder is reached through a
/// link, and results should show the path the user knows).
fn lexical_abs(path: &std::path::Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;
    // Relative paths start from the directory as the shell names it ($PWD,
    // like `pwd -L`), when that really is the current directory.
    let logical_cwd = std::env::var_os("PWD").map(PathBuf::from).filter(|p| {
        p.is_absolute()
            && matches!((std::fs::metadata(p), std::fs::metadata(".")), (Ok(a), Ok(b)) if a.dev() == b.dev() && a.ino() == b.ino())
    });
    let abs = match (path.is_absolute(), logical_cwd) {
        (false, Some(cwd)) => cwd.join(path),
        _ => std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()),
    };
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

async fn account_for(
    c: &zbus::Connection,
    m: &ManagerProxy<'_>,
    path: &std::path::Path,
) -> anyhow::Result<(AccountProxy<'static>, String)> {
    let abs = lexical_abs(path);
    let s = abs.to_string_lossy().into_owned();
    let infos: Vec<PathInfo> = serde_json::from_str(&m.paths_info(&[s.as_str()]).await?)?;
    let acct = infos
        .first()
        .map(|i| i.account.clone())
        .filter(|a| !a.is_empty())
        .ok_or_else(|| anyhow!("{s} is not inside a OneDrive folder"))?;
    Ok((account_proxy(c, &acct).await?, s))
}

fn parse_duration(s: &str) -> anyhow::Result<u32> {
    let d = humantime::parse_duration(s).with_context(|| format!("invalid duration {s:?} (try 2h, 8h, 24h)"))?;
    Ok(d.as_secs().min(u32::MAX as u64) as u32)
}

fn abs_paths(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|p| lexical_abs(p).to_string_lossy().into_owned()).collect()
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Exit quietly when the reader goes away (`… | head`), like other tools.
    // SAFETY: called once at startup, before any other thread exists.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let c = conn().await?;
    let m = manager(&c).await?;
    let acct_sel = cli.account.as_deref();
    match cli.cmd {
        Cmd::Status => {
            let state = m.state().await?;
            let paused = m.paused_until().await?;
            if cli.json {
                let mut accts = Vec::new();
                for p in m.accounts().await? {
                    let a = account_proxy(&c, p.as_str()).await?;
                    accts.push(serde_json::json!({
                        "id": a.id().await?, "email": a.email().await?, "state": a.state().await?,
                        "status": a.status_text().await?, "mount_point": a.mount_point().await?,
                        "quota_used": a.quota_used().await?, "quota_total": a.quota_total().await?,
                        "issues": a.issue_count().await?, "held_deletes": a.held_deletes().await?,
                        "progress": serde_json::from_str::<serde_json::Value>(&a.progress().await?).unwrap_or_default(),
                    }));
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"state": state, "paused_until": paused, "accounts": accts})
                    )?
                );
                return Ok(());
            }
            println!("Linux OneDrive {}: {}", m.version().await?, state);
            if paused > 0 {
                let t = chrono::DateTime::from_timestamp(paused, 0).unwrap_or_default().with_timezone(&chrono::Local);
                println!("Paused until {}", t.format("%H:%M"));
            } else if paused < 0 {
                println!("Paused until resumed");
            }
            for p in m.accounts().await? {
                let a = account_proxy(&c, p.as_str()).await?;
                let prog: Progress = serde_json::from_str(&a.progress().await?).unwrap_or_default();
                println!();
                println!("{} ({})", a.email().await?, a.mount_point().await?);
                println!("  {}", a.status_text().await?);
                let (used, total) = (a.quota_used().await?, a.quota_total().await?);
                if total > 0 {
                    println!("  Storage: {} of {} used", od_core::fmt::size(used), od_core::fmt::size(total));
                }
                if prog.files_pending() > 0 {
                    println!("  Pending: {} upload(s), {} download(s)", prog.uploads_pending, prog.downloads_pending);
                }
                let issues = a.issue_count().await?;
                if issues > 0 {
                    println!("  {issues} item(s) can't sync. Run `linux-onedrivectl issues`");
                }
                let held = a.held_deletes().await?;
                if held > 0 {
                    println!(
                        "  {held} deletion(s) wait for confirmation. Run `linux-onedrivectl mass-delete --delete|--restore`"
                    );
                }
            }
        }
        Cmd::Accounts => {
            for p in m.accounts().await? {
                let a = account_proxy(&c, p.as_str()).await?;
                let org = a.organization().await?;
                println!(
                    "{}  {}  {}  {}",
                    a.id().await?,
                    a.email().await?,
                    if org.is_empty() { "Personal".into() } else { org },
                    a.mount_point().await?
                );
            }
        }
        Cmd::Login { email, device, folder } => {
            let mut finished = m.receive_sign_in_finished().await?;
            if device {
                let (code, uri, message) = m.begin_device_sign_in().await?;
                println!(
                    "{}",
                    if message.is_empty() { format!("Go to {uri} and enter the code {code}") } else { message }
                );
            } else {
                let (url, _) = m.begin_sign_in(email.as_deref().unwrap_or(""), "browser").await?;
                println!("Opening your browser to sign in. If it doesn't open, visit:\n{url}");
                let _ = std::process::Command::new("xdg-open").arg(&url).status();
            }
            println!("Waiting for sign-in to finish…");
            let sig = finished.next().await.ok_or_else(|| anyhow!("the daemon went away"))?;
            let args = sig.args()?;
            if !args.ok {
                bail!("sign-in failed: {}", args.message);
            }
            let a = account_proxy(&c, args.account.as_str()).await?;
            if a.needs_setup().await? {
                if let Some(f) = folder {
                    let f = std::path::absolute(&f)?;
                    a.set_mount_point(&f.to_string_lossy()).await?;
                }
                a.start().await?;
            }
            println!("Signed in as {}. Your files are in {}", a.email().await?, a.mount_point().await?);
        }
        Cmd::Logout => {
            let a = pick_account(&c, &m, acct_sel).await?;
            let email = a.email().await?;
            a.sign_out().await?;
            println!("Unlinked {email}");
        }
        Cmd::Pause { duration } => {
            let secs = match duration {
                Some(d) => parse_duration(&d)?,
                None => 0,
            };
            m.pause(secs).await?;
            println!(
                "{}",
                if secs == 0 {
                    "Paused until resumed".to_string()
                } else {
                    format!("Paused for {}", humantime::format_duration(std::time::Duration::from_secs(secs as u64)))
                }
            );
        }
        Cmd::Resume => {
            m.resume().await?;
            println!("Syncing resumed");
        }
        Cmd::Pin { paths } | Cmd::Unpin { paths } | Cmd::Free { paths } if paths.is_empty() => {
            bail!("give at least one path")
        }
        cmd @ (Cmd::Pin { .. } | Cmd::Unpin { .. } | Cmd::Free { .. }) => {
            let (paths, state) = match cmd {
                Cmd::Pin { paths } => (paths, "pinned"),
                Cmd::Unpin { paths } => (paths, "inherit"),
                Cmd::Free { paths } => (paths, "unpinned"),
                _ => unreachable!(),
            };
            let (a, _) = account_for(&c, &m, &paths[0]).await?;
            let abs = abs_paths(&paths);
            let refs: Vec<&str> = abs.iter().map(String::as_str).collect();
            a.set_pin_state(&refs, state).await?;
        }
        Cmd::Info { paths } => {
            let abs = abs_paths(&paths);
            let refs: Vec<&str> = abs.iter().map(String::as_str).collect();
            let infos: Vec<PathInfo> = serde_json::from_str(&m.paths_info(&refs).await?)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&infos)?);
            } else {
                for i in infos {
                    let status = if i.account.is_empty() { "not in OneDrive".to_string() } else { i.status.clone() };
                    let extra = if i.error.is_empty() { String::new() } else { format!(" ({})", i.error) };
                    println!("{:<9} {:<9} {}{}", status, i.pin, i.path, extra);
                }
            }
        }
        Cmd::Share { path, edit, view, org, people, expires, password, replace } => {
            let (a, p) = account_for(&c, &m, &path).await?;
            let plain = !edit && !view && !org && people.is_empty() && expires.is_none() && password.is_none();
            if plain {
                println!("{}", a.copy_link(&p).await?);
                return Ok(());
            }
            let info: SharingInfo = serde_json::from_str(&a.get_sharing_info(&p).await?)?;
            if !info.can_share {
                bail!("{}", info.reason);
            }
            let opts = LinkOptions {
                audience: if !people.is_empty() {
                    "users".into()
                } else if org {
                    "organization".into()
                } else if edit || view {
                    "anyone".into()
                } else {
                    info.default_audience.clone()
                },
                role: if edit {
                    "edit".into()
                } else if view {
                    "view".into()
                } else {
                    info.default_role.clone()
                },
                expires: expires.map(|d| chrono::Utc::now().timestamp() + i64::from(d) * 86_400).unwrap_or(0),
                password: password.unwrap_or_default(),
                recipients: people,
                notify: false,
                replace,
            };
            let r: LinkResult = serde_json::from_str(&a.create_link(&p, &serde_json::to_string(&opts)?).await?)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                println!("{}", r.url);
                if r.existing {
                    eprintln!(
                        "note: this is the existing link, which has other settings; add --replace for a new one \
                         (people who have this link will lose access)"
                    );
                } else if !r.settings_applied {
                    eprintln!(
                        "note: {}",
                        if r.message.is_empty() { "some link settings couldn't be applied" } else { &r.message }
                    );
                }
            }
        }
        Cmd::Invite { path, emails, edit, message, no_email } => {
            let (a, p) = account_for(&c, &m, &path).await?;
            let opts = InviteOptions {
                recipients: emails,
                role: if edit { "edit" } else { "view" }.into(),
                message: message.unwrap_or_default(),
                notify: !no_email,
                ..Default::default()
            };
            let out: InviteOutcome = serde_json::from_str(&a.invite(&p, &serde_json::to_string(&opts)?).await?)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!("Shared with {} {}", out.granted, if out.granted == 1 { "person" } else { "people" });
                for f in &out.failed {
                    eprintln!("{}: {}", f.email, f.message);
                }
            }
            if !out.failed.is_empty() {
                std::process::exit(1);
            }
        }
        Cmd::Access { path } => {
            let (a, p) = account_for(&c, &m, &path).await?;
            let list: Vec<AccessEntry> = serde_json::from_str(&a.list_access(&p).await?)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else {
                for e in &list {
                    let who = match e.kind.as_str() {
                        "link" => match e.audience.as_str() {
                            "organization" => "link: people in your organization".to_string(),
                            "users" => format!(
                                "link: {}",
                                e.people.iter().map(|p| p.email.as_str()).collect::<Vec<_>>().join(", ")
                            ),
                            "existing" => "link: people with existing access".to_string(),
                            _ => "link: anyone".to_string(),
                        },
                        _ => e
                            .people
                            .first()
                            .map(|p| if p.email.is_empty() { p.name.clone() } else { p.email.clone() })
                            .unwrap_or_default(),
                    };
                    let mut notes = Vec::new();
                    if e.expires > 0 {
                        notes.push(format!(
                            "expires {}",
                            chrono::DateTime::from_timestamp(e.expires, 0)
                                .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
                                .unwrap_or_default()
                        ));
                    }
                    if e.has_password {
                        notes.push("password".into());
                    }
                    if !e.inherited_from.is_empty() {
                        notes.push(format!("from {}", e.inherited_from));
                    }
                    let id = if e.removable { e.id.as_str() } else { "-" };
                    println!(
                        "{:<20} {:<7} {}{}{}",
                        id,
                        e.role,
                        who,
                        if notes.is_empty() { String::new() } else { format!("  ({})", notes.join(", ")) },
                        if e.url.is_empty() { String::new() } else { format!("\n{:<28} {}", "", e.url) }
                    );
                }
            }
        }
        Cmd::Unshare { path, id, all } => {
            let (a, p) = account_for(&c, &m, &path).await?;
            if all {
                let out: StopSharingOutcome = serde_json::from_str(&a.stop_sharing(&p).await?)?;
                println!("Removed {} {}", out.removed, if out.removed == 1 { "entry" } else { "entries" });
                for f in &out.failed {
                    eprintln!("{f}");
                }
            } else if let Some(id) = id {
                a.remove_access(&p, &id).await?;
            } else {
                bail!("give an id from `linux-onedrivectl access {}` or --all", path.display());
            }
        }
        Cmd::Link { path } => {
            let (a, p) = account_for(&c, &m, &path).await?;
            println!("{}", a.web_url(&p).await?);
        }
        Cmd::Versions { path } => {
            let (a, p) = account_for(&c, &m, &path).await?;
            let list: Vec<VersionInfo> = serde_json::from_str(&a.list_versions(&p).await?)?;
            for v in list {
                let t =
                    chrono::DateTime::from_timestamp(v.modified, 0).unwrap_or_default().with_timezone(&chrono::Local);
                println!(
                    "{:<12} {}  {:>10}  {}{}",
                    v.id,
                    t.format("%Y-%m-%d %H:%M"),
                    od_core::fmt::size(v.size),
                    v.modified_by,
                    if v.current { "  (current)" } else { "" }
                );
            }
        }
        Cmd::RestoreVersion { path, version } => {
            let (a, p) = account_for(&c, &m, &path).await?;
            a.restore_version(&p, &version).await?;
            println!("Restored version {version}");
        }
        Cmd::Locations => {
            let a = pick_account(&c, &m, acct_sel).await?;
            let list: Vec<SyncLocation> = serde_json::from_str(&a.list_locations().await?)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else if list.is_empty() {
                println!("No shared folders or libraries.");
            } else {
                for l in list {
                    println!("{:<24} {:<8} {:<10} {}\n{:<24} {}", l.key, l.kind, l.state, l.title, "", l.local_path);
                }
            }
        }
        Cmd::RemoveLocation { key } => {
            let a = pick_account(&c, &m, acct_sel).await?;
            a.remove_location(&key).await?;
        }
        Cmd::Sites { query } => {
            let a = pick_account(&c, &m, acct_sel).await?;
            let list: Vec<SiteInfo> = serde_json::from_str(&a.search_sites(query.as_deref().unwrap_or("")).await?)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else {
                for s in list {
                    println!("{}  {}  {}", s.id, s.name, s.web_url);
                }
            }
        }
        Cmd::Libraries { site_id } => {
            let a = pick_account(&c, &m, acct_sel).await?;
            let list: Vec<LibraryInfo> = serde_json::from_str(&a.site_libraries(&site_id).await?)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else {
                for l in list {
                    println!("{}  {}{}", l.drive_id, l.name, if l.synced { "  (syncing)" } else { "" });
                }
            }
        }
        Cmd::AddLibrary { site_id, drive_id } => {
            let a = pick_account(&c, &m, acct_sel).await?;
            println!("{}", a.add_library(&site_id, &drive_id).await?);
        }
        Cmd::OpenLink { url } => {
            println!("{}", m.open_sync_link(&url).await?);
        }
        Cmd::Update { install } => {
            let u: od_core::update::UpdateInfo = serde_json::from_str(&m.check_for_update(true).await?)?;
            if cli.json && !install {
                println!("{}", serde_json::to_string_pretty(&u)?);
                return Ok(());
            }
            if !u.error.is_empty() {
                bail!("couldn't check for updates: {}", u.error);
            }
            if !u.available {
                println!("{} {} is the latest version.", od_core::APP_NAME, u.current);
                return Ok(());
            }
            println!("Version {} is available (you have {}).", u.latest, u.current);
            if !u.notes.trim().is_empty() {
                println!("\nWhat's new:\n{}\n", u.notes.trim());
            }
            if !install {
                println!("Install it with: linux-onedrivectl update --install\nOr download it from {}", u.page_url);
                return Ok(());
            }
            if !u.installable {
                bail!("this release has no installable package for this system; see {}", u.page_url);
            }
            println!("Downloading and checking the update…");
            let path = m.download_update().await?;
            // In a terminal sudo asks there; otherwise use the desktop's prompt.
            use std::io::IsTerminal;
            let (tool, args): (&str, Vec<&str>) = if std::io::stdin().is_terminal() {
                ("sudo", vec!["apt-get", "install", "-y", &path])
            } else {
                ("pkexec", vec!["/usr/bin/apt-get", "install", "-y", &path])
            };
            let status = std::process::Command::new(tool)
                .args(&args)
                .status()
                .with_context(|| format!("running {tool}; install by hand with: sudo apt install \"{path}\""))?;
            if !status.success() {
                bail!("the update was not installed (the package is at {path})");
            }
            // Run the new version: restart the service, however it was started.
            let systemctl = |a: &[&str]| std::process::Command::new("systemctl").arg("--user").args(a).status();
            let _ = systemctl(&["daemon-reload"]);
            if systemctl(&["is-active", "--quiet", "linux-onedrived.service"]).is_ok_and(|s| s.success()) {
                let _ = systemctl(&["restart", "linux-onedrived.service"]);
            } else {
                let _ = m.quit().await;
            }
            println!("Updated to version {}. Quit and reopen the Linux OneDrive app to finish.", u.latest);
        }
        Cmd::Activity { n } => {
            let a = pick_account(&c, &m, acct_sel).await?;
            let list: Vec<ActivityEntry> = serde_json::from_str(&a.recent_activity(n).await?)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else {
                for e in list {
                    let t = chrono::DateTime::from_timestamp(e.time, 0).unwrap_or_default();
                    println!(
                        "{:<16} {} {}: {}",
                        od_core::fmt::relative_time(t, chrono::Utc::now()),
                        e.kind.verb(),
                        e.folder,
                        e.name
                    );
                }
            }
        }
        Cmd::Issues { retry } => {
            let a = pick_account(&c, &m, acct_sel).await?;
            if retry {
                a.retry_failed().await?;
                println!("Retrying failed items");
                return Ok(());
            }
            let list: Vec<Issue> = serde_json::from_str(&a.list_issues().await?)?;
            if list.is_empty() {
                println!("No sync issues");
            }
            for i in list {
                println!("{}\n    {}", i.path, i.message);
            }
        }
        Cmd::MassDelete { delete, restore } => {
            if !delete && !restore {
                bail!("choose --delete or --restore");
            }
            let a = pick_account(&c, &m, acct_sel).await?;
            let n = a.held_deletes().await?;
            if n == 0 {
                println!("No deletions are waiting");
                return Ok(());
            }
            a.resolve_mass_delete(delete).await?;
            println!(
                "{}",
                if delete { format!("Deleting {n} item(s) online") } else { format!("Restoring {n} item(s)") }
            );
        }
        Cmd::Backup { action, folder } => {
            let a = pick_account(&c, &m, acct_sel).await?;
            match (action.as_deref(), folder) {
                (None, _) | (Some("list"), _) => {
                    let list: Vec<od_core::dbus::BackupFolder> = serde_json::from_str(&a.list_backup_folders().await?)?;
                    for f in list {
                        let state = if f.in_progress {
                            "updating"
                        } else if f.backed_up {
                            "backed up"
                        } else {
                            "not backed up"
                        };
                        println!(
                            "{:<10} {:<14} {:>6} files {:>10}  {}",
                            f.name,
                            state,
                            f.files,
                            od_core::fmt::size(f.bytes),
                            f.local_path
                        );
                    }
                }
                (Some(act @ ("start" | "stop")), Some(folder)) => {
                    a.set_folder_backup(&folder.to_ascii_uppercase(), act == "start").await?;
                    println!(
                        "{} backup of {folder}; you'll get a notification when it's done",
                        if act == "start" { "Starting" } else { "Stopping" }
                    );
                }
                _ => bail!("usage: backup [list] | backup start <folder> | backup stop <folder>"),
            }
        }
        Cmd::Removed => {
            let a = pick_account(&c, &m, acct_sel).await?;
            let list: Vec<od_core::dbus::RemovedItem> = serde_json::from_str(&a.list_removed().await?)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else if list.is_empty() {
                println!("Nothing to restore");
            } else {
                for r in list {
                    let t = chrono::DateTime::from_timestamp(r.removed_at, 0).unwrap_or_default();
                    println!(
                        "{}  {:>10}  {:<16} {}",
                        r.id,
                        od_core::fmt::size(r.size),
                        od_core::fmt::relative_time(t, chrono::Utc::now()),
                        r.rel_path
                    );
                }
            }
        }
        Cmd::RestoreRemoved { id } => {
            let a = pick_account(&c, &m, acct_sel).await?;
            println!("Restored to {}", a.restore_removed(&id).await?);
        }
        Cmd::Refresh => {
            pick_account(&c, &m, acct_sel).await?.refresh().await?;
        }
        Cmd::Settings { assignments } => {
            if assignments.is_empty() {
                let v: serde_json::Value = serde_json::from_str(&m.get_settings().await?)?;
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            let mut patch = serde_json::Map::new();
            for a in assignments {
                let (key, val) = a.split_once('=').ok_or_else(|| anyhow!("expected key=value, got {a:?}"))?;
                let value: serde_json::Value =
                    serde_json::from_str(val).unwrap_or(serde_json::Value::String(val.to_string()));
                let mut parts: Vec<&str> = key.split('.').collect();
                let last = parts.pop().ok_or_else(|| anyhow!("empty key"))?;
                let mut cur = &mut patch;
                for p in parts {
                    cur = cur
                        .entry(p.to_string())
                        .or_insert_with(|| serde_json::Value::Object(Default::default()))
                        .as_object_mut()
                        .ok_or_else(|| anyhow!("{key}: not an object"))?;
                }
                cur.insert(last.to_string(), value);
            }
            m.set_settings(&serde_json::Value::Object(patch).to_string()).await?;
            println!("Saved");
        }
        Cmd::Quit => {
            m.quit().await?;
            println!("Stopping the sync service");
        }
    }
    Ok(())
}
