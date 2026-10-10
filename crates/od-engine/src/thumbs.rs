//! Previews for online-only files, like the official client shows them.
//!
//! GIO considers our FUSE mount local, so Files would normally run a
//! thumbnailer, which reads (downloads) the whole file. Instead, when a
//! folder is listed, we fetch OneDrive's server-side thumbnail for its
//! online-only images, videos and PDFs and write it into the freedesktop
//! thumbnail cache (`~/.cache/thumbnails/{large,x-large}/<md5(uri)>.png`,
//! with `Thumb::URI`/`Thumb::MTime` matching what we report). Files then
//! shows a real preview and never asks a thumbnailer.
//!
//! When OneDrive has no thumbnail we write a "failed" marker so nothing
//! tries again; it is removed once the file is downloaded, so GNOME can make
//! a real thumbnail from the local copy.

use crate::engine::Engine;
use crate::store::{self, Item};
use md5::{Digest, Md5};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Extensions GNOME thumbnails by default (images, video, PDF).
const THUMB_EXTS: &[&str] = &[
    ".jpg", ".jpeg", ".png", ".gif", ".webp", ".heic", ".heif", ".avif", ".tif", ".tiff", ".bmp", ".jxl", ".mp4",
    ".m4v", ".mov", ".mkv", ".webm", ".avi", ".3gp", ".wmv", ".pdf",
];

/// (directory, longest edge) for the sizes Nautilus asks for.
const SIZES: &[(&str, u32)] = &[("large", 256), ("x-large", 512)];
const FAIL_DIR: &str = "fail/gnome-thumbnail-factory";

#[derive(Default)]
pub struct ThumbQueue {
    queue: Mutex<VecDeque<u64>>,
    queued: Mutex<HashSet<u64>>,
    pub(crate) wake: tokio::sync::Notify,
}

pub fn wants_thumbnail(name: &str) -> bool {
    let ext = od_core::names::split_ext(name).1.to_ascii_lowercase();
    THUMB_EXTS.contains(&ext.as_str())
}

/// `file://` URI exactly as GLib's `g_filename_to_uri` builds it (the cache
/// file name is the MD5 of this string, so it must match byte for byte).
pub fn glib_file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        let keep = b.is_ascii_alphanumeric() || b"!$&'()*+,-./:=@_~".contains(&b);
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn cache_root() -> PathBuf {
    match std::env::var_os("XDG_CACHE_HOME") {
        Some(v) if Path::new(&v).is_absolute() => PathBuf::from(v).join("thumbnails"),
        _ => od_core::paths::home_dir().join(".cache/thumbnails"),
    }
}

pub fn md5_hex(s: &str) -> String {
    Md5::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Is there already a valid thumbnail (or fail marker) for this URI and mtime?
fn has_valid(path: &Path, uri: &str, mtime: i64) -> bool {
    let Ok(f) = std::fs::File::open(path) else { return false };
    let decoder = png::Decoder::new(std::io::BufReader::new(f));
    let Ok(reader) = decoder.read_info() else { return false };
    let info = reader.info();
    let get = |k: &str| info.uncompressed_latin1_text.iter().find(|t| t.keyword == k).map(|t| t.text.clone());
    get("Thumb::URI").as_deref() == Some(uri) && get("Thumb::MTime") == Some(mtime.to_string())
}

/// Write an RGBA PNG with the freedesktop metadata (temp file + rename, 0600).
fn write_png(dest: &Path, rgba: &[u8], w: u32, h: u32, uri: &str, mtime: i64, size: u64) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = dest.parent().expect("thumbnail dir");
    od_core::paths::ensure_private_dir(dir)?;
    let tmp = dest.with_extension(format!("tmp{}", std::process::id()));
    {
        let f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let text = |e: &mut png::Encoder<_>, k: &str, v: String| {
            e.add_text_chunk(k.to_string(), v).map_err(std::io::Error::other)
        };
        text(&mut enc, "Thumb::URI", uri.to_string())?;
        text(&mut enc, "Thumb::MTime", mtime.to_string())?;
        text(&mut enc, "Thumb::Size", size.to_string())?;
        text(&mut enc, "Software", "linux-onedrive".to_string())?;
        let mut writer = enc.write_header().map_err(std::io::Error::other)?;
        writer.write_image_data(rgba).map_err(std::io::Error::other)?;
        writer.finish().map_err(std::io::Error::other)?;
    }
    std::fs::rename(tmp, dest)
}

impl Engine {
    /// Queue thumbnails for the online-only media files in a folder that
    /// a file manager just listed.
    pub(crate) fn queue_thumbnails(&self, children: &[Item]) {
        if !self.cfg().server_thumbnails {
            return;
        }
        let mut added = false;
        {
            let mut q = self.thumbs.queue.lock().unwrap();
            let mut queued = self.thumbs.queued.lock().unwrap();
            for it in children.iter().take(500) {
                if it.is_dir || it.hydrated || it.item_id.is_none() || !wants_thumbnail(&it.name) {
                    continue;
                }
                if queued.insert(it.ino) {
                    q.push_back(it.ino);
                    added = true;
                }
            }
        }
        if added {
            self.thumbs.wake.notify_one();
        }
    }

    pub(crate) async fn thumbnail_loop(self: Arc<Self>) {
        loop {
            let next = self.thumbs.queue.lock().unwrap().pop_front();
            let Some(ino) = next else {
                tokio::select! {
                    _ = self.cancel.cancelled() => return,
                    _ = self.thumbs.wake.notified() => continue,
                }
            };
            self.thumbs.queued.lock().unwrap().remove(&ino);
            if self.is_paused() || self.is_offline() || self.is_auth_required() {
                continue;
            }
            if let Err(e) = self.make_thumbnail(ino).await {
                tracing::debug!(ino, "no server thumbnail: {e}");
            }
        }
    }

    async fn make_thumbnail(&self, ino: u64) -> anyhow::Result<()> {
        let Some(it) = store::get_live(&self.store.lock(), ino)? else { return Ok(()) };
        let Some(item_id) = it.item_id.clone() else { return Ok(()) };
        if it.hydrated {
            return Ok(()); // GNOME thumbnails local files itself
        }
        let path = self.local_path(ino);
        let uri = glib_file_uri(&path);
        let mtime = it.mtime_ns.div_euclid(1_000_000_000);
        let name = format!("{}.png", md5_hex(&uri));
        let root = self.cfg().thumbnail_root;
        if SIZES.iter().all(|(dir, _)| has_valid(&root.join(dir).join(&name), &uri, mtime))
            || has_valid(&root.join(FAIL_DIR).join(&name), &uri, mtime)
        {
            return Ok(());
        }
        let sets = self.graph.thumbnails(&item_id).await?;
        let url = sets.first().and_then(|s| s.large.as_ref().or(s.medium.as_ref()).and_then(|t| t.url.clone()));
        let Some(url) = url else {
            // No preview online: remember that, so no thumbnailer downloads it.
            write_png(&root.join(FAIL_DIR).join(&name), &[0, 0, 0, 0], 1, 1, &uri, mtime, it.size)?;
            return Ok(());
        };
        let bytes = self.graph.fetch_unauthenticated(&url).await?;
        let img = tokio::task::spawn_blocking(move || image::load_from_memory(&bytes)).await??;
        for (dir, edge) in SIZES {
            let scaled =
                if img.width() > *edge || img.height() > *edge { img.thumbnail(*edge, *edge) } else { img.clone() };
            let rgba = scaled.to_rgba8();
            write_png(&root.join(dir).join(&name), rgba.as_raw(), rgba.width(), rgba.height(), &uri, mtime, it.size)?;
        }
        Ok(())
    }

    /// A file was downloaded: drop its "failed" marker so GNOME makes a real
    /// thumbnail from the local copy.
    pub(crate) fn clear_thumbnail_failure(&self, ino: u64) {
        let path = self.local_path(ino);
        let name = format!("{}.png", md5_hex(&glib_file_uri(&path)));
        let _ = std::fs::remove_file(self.cfg().thumbnail_root.join(FAIL_DIR).join(name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_matches_glib() {
        // Reference produced by GLib.filename_to_uri on this system.
        let p = Path::new("/home/u/OneDrive/Fotos & Co/ä b(1)!~'*;=,@$+#%[x]{y}.jpg");
        assert_eq!(
            glib_file_uri(p),
            "file:///home/u/OneDrive/Fotos%20&%20Co/%C3%A4%20b(1)!~'*%3B=,@$+%23%25%5Bx%5D%7By%7D.jpg"
        );
        assert!(wants_thumbnail("IMG_001.JPG") && wants_thumbnail("doc.pdf") && !wants_thumbnail("notes.txt"));
    }

    #[test]
    fn png_metadata_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x/t.png");
        write_png(&p, &[255, 0, 0, 255], 1, 1, "file:///a", 42, 7).unwrap();
        assert!(has_valid(&p, "file:///a", 42));
        assert!(!has_valid(&p, "file:///a", 43));
        assert!(!has_valid(&p, "file:///b", 42));
    }
}
