//! Library scanning.
//!
//! Phase 1 (fast): walk the tree, compare path/size/mtime with the database, record new, changed
//! and removed files. No file contents are read. Phase 2 (slower, resumable): for every `pending`
//! file compute the BLAKE3 hash and read dimensions/EXIF. Files become eligible for display as
//! phase 2 completes them, so the server is useful immediately after start.

use crate::config::Config;
use crate::db::{bump_generation, now_unix, Db};
use anyhow::{bail, Result};
use rusqlite::params;
use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};
use walkdir::WalkDir;

#[derive(Default)]
pub struct ScanState {
    pub running: AtomicBool,
    queued: AtomicBool,
    queued_full: AtomicBool,
    pub first_walk_done: AtomicBool,
    pub scans_total: AtomicU64,
    pub last_started: AtomicI64,
    pub last_finished: AtomicI64,
    pub last_walk_ms: AtomicU64,
    pub last_total_ms: AtomicU64,
    pub last_found: AtomicU64,
    pub last_new: AtomicU64,
    pub last_changed: AtomicU64,
    pub last_removed: AtomicU64,
    pub last_unchanged: AtomicU64,
    pub enriched_ok_total: AtomicU64,
    pub enriched_err_total: AtomicU64,
    pub last_error: Mutex<Option<String>>,
}

#[derive(Debug, Default, Clone)]
pub struct WalkStats {
    pub found: u64,
    pub new: u64,
    pub changed: u64,
    pub removed: u64,
    pub unchanged: u64,
    pub walk_errors: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Started,
    Queued,
}

pub struct Scanner {
    pub state: Arc<ScanState>,
    db: Db,
    cfg: Arc<Config>,
}

impl Scanner {
    pub fn new(db: Db, cfg: Arc<Config>) -> Arc<Scanner> {
        Arc::new(Scanner {
            state: Arc::new(ScanState::default()),
            db,
            cfg,
        })
    }

    /// Start a scan in the background, or queue one if a scan is already running.
    pub fn trigger(self: &Arc<Self>, full: bool) -> Trigger {
        if self
            .state
            .running
            .compare_exchange(false, true, Relaxed, Relaxed)
            .is_ok()
        {
            let me = self.clone();
            tokio::spawn(async move { me.run_loop(full).await });
            Trigger::Started
        } else {
            if full {
                self.state.queued_full.store(true, Relaxed);
            }
            self.state.queued.store(true, Relaxed);
            Trigger::Queued
        }
    }

    async fn run_loop(self: Arc<Self>, mut full: bool) {
        loop {
            if let Err(e) = self.run_once(full).await {
                error!("scan failed: {e:#}");
                *self.state.last_error.lock().unwrap() = Some(format!("{e:#}"));
            }
            if self.state.queued.swap(false, Relaxed) {
                full = self.state.queued_full.swap(false, Relaxed);
                continue;
            }
            self.state.running.store(false, Relaxed);
            // A trigger may have queued itself between the check above and clearing `running`.
            if self.state.queued.load(Relaxed)
                && self
                    .state
                    .running
                    .compare_exchange(false, true, Relaxed, Relaxed)
                    .is_ok()
            {
                self.state.queued.store(false, Relaxed);
                full = self.state.queued_full.swap(false, Relaxed);
                continue;
            }
            break;
        }
    }

    async fn run_once(&self, full: bool) -> Result<()> {
        let st = &self.state;
        let t0 = Instant::now();
        st.last_started.store(now_unix(), Relaxed);
        info!(full, dir = %self.cfg.library.dir, "scan started");

        let (db, cfg) = (self.db.clone(), self.cfg.clone());
        let w = tokio::task::spawn_blocking(move || walk_and_upsert(&db, &cfg, full)).await??;
        st.first_walk_done.store(true, Relaxed);
        st.last_walk_ms
            .store(t0.elapsed().as_millis() as u64, Relaxed);
        st.last_found.store(w.found, Relaxed);
        st.last_new.store(w.new, Relaxed);
        st.last_changed.store(w.changed, Relaxed);
        st.last_removed.store(w.removed, Relaxed);
        st.last_unchanged.store(w.unchanged, Relaxed);
        *st.last_error.lock().unwrap() = None;
        info!(
            found = w.found,
            new = w.new,
            changed = w.changed,
            removed = w.removed,
            unchanged = w.unchanged,
            walk_errors = w.walk_errors,
            ms = t0.elapsed().as_millis() as u64,
            "walk finished"
        );

        let mut processed = 0u64;
        loop {
            let (db, cfg, state) = (self.db.clone(), self.cfg.clone(), self.state.clone());
            let n = tokio::task::spawn_blocking(move || enrich_batch(&db, &cfg, &state)).await??;
            if n == 0 {
                break;
            }
            processed += n as u64;
            if processed % 2048 < n as u64 {
                info!(processed, "hashing/indexing progress");
            }
        }

        // Without HEIC support those files are indexed as "unsupported"; say so once per scan, with the fix.
        if !cfg!(feature = "heic") {
            let skipped: i64 = self
                .db
                .run(|c| {
                    Ok(c.query_row(
                        "SELECT COUNT(*) FROM files WHERE missing = 0 AND status = 'unsupported' AND
                         (lower(path) LIKE '%.heic' OR lower(path) LIKE '%.heif' OR lower(path) LIKE '%.hif')",
                        [],
                        |r| r.get(0),
                    )?)
                })
                .await?;
            if skipped > 0 {
                warn!(
                    count = skipped,
                    "HEIC/HEIF files are being skipped: {}",
                    crate::imaging::NO_HEIC_MSG
                );
            }
        }

        // Housekeeping: forget files that have been gone for a month and old history.
        let db = self.db.clone();
        db.run(|c| {
            let now = now_unix();
            c.execute(
                "DELETE FROM files WHERE missing = 1 AND last_seen < ?1",
                [now - 30 * 86400],
            )?;
            c.execute(
                "DELETE FROM history WHERE served_at < ?1",
                [now - 90 * 86400],
            )?;
            Ok(())
        })
        .await?;

        st.scans_total.fetch_add(1, Relaxed);
        st.last_total_ms
            .store(t0.elapsed().as_millis() as u64, Relaxed);
        st.last_finished.store(now_unix(), Relaxed);
        info!(
            processed,
            secs = t0.elapsed().as_secs_f32(),
            "scan finished"
        );
        Ok(())
    }
}

fn is_skipped_dir(name: &str, skip: &HashSet<&str>) -> bool {
    name.starts_with('.') || skip.contains(name)
}

pub fn walk_and_upsert(db: &Db, cfg: &Config, full: bool) -> Result<WalkStats> {
    let root = Path::new(&cfg.library.dir);
    let exts: HashSet<String> = cfg
        .library
        .extensions
        .iter()
        .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
        .collect();
    let skip: HashSet<&str> = cfg.library.skip_dirs.iter().map(|s| s.as_str()).collect();
    let mut stats = WalkStats::default();

    let mut found: Vec<(String, i64, i64)> = Vec::new();
    let walker = WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || !e.file_type().is_dir()
                || !is_skipped_dir(&e.file_name().to_string_lossy(), &skip)
        });
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                warn!("walk error: {err}");
                stats.walk_errors += 1;
                continue;
            }
        };
        if !entry.file_type().is_file() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let ext_ok = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| exts.contains(&e.to_ascii_lowercase()));
        if !ext_ok {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let Some(rel) = rel.to_str() else {
            warn!("skipping non-UTF-8 path {:?}", entry.path());
            continue;
        };
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                warn!("cannot stat {rel}: {e}");
                stats.walk_errors += 1;
                continue;
            }
        };
        let mtime_ns = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as i64);
        found.push((rel.replace('\\', "/"), md.len() as i64, mtime_ns));
    }
    stats.found = found.len() as u64;

    db.with(|conn| {
        let tx = conn.transaction()?;
        let mut existing: HashMap<String, (i64, i64, i64, bool)> = HashMap::new();
        {
            let mut st = tx.prepare("SELECT id, path, size, mtime_ns, missing FROM files")?;
            let rows = st.query_map([], |r| {
                Ok((r.get::<_, String>(1)?, (r.get(0)?, r.get(2)?, r.get(3)?, r.get::<_, i64>(4)? != 0)))
            })?;
            for r in rows {
                let (p, v) = r?;
                existing.insert(p, v);
            }
        }
        // Safety net: an unmounted/empty library must not wipe the index.
        let live = existing.values().filter(|v| !v.3).count();
        if found.is_empty() && live > 0 {
            bail!("library directory {} yielded no images but the index has {live}; refusing to mark them missing (is the volume mounted?)", cfg.library.dir);
        }

        let now = now_unix();
        let mut changed_any = false;
        {
            let mut ins = tx.prepare(
                "INSERT INTO files(path,size,mtime_ns,status,first_seen,last_seen) VALUES(?1,?2,?3,'pending',?4,?4)",
            )?;
            let mut upd = tx.prepare(
                "UPDATE files SET size=?2, mtime_ns=?3, hash=NULL, phash=NULL, width=NULL, height=NULL, taken_at=NULL,
                        format=NULL, status='pending', error=NULL, missing=0, last_seen=?4 WHERE id=?1",
            )?;
            let mut revive = tx.prepare("UPDATE files SET missing=0, last_seen=?2 WHERE id=?1")?;
            for (path, size, mtime) in &found {
                match existing.remove(path) {
                    None => {
                        ins.execute(params![path, size, mtime, now])?;
                        stats.new += 1;
                        changed_any = true;
                    }
                    Some((id, osize, omtime, was_missing)) => {
                        if full || osize != *size || omtime != *mtime {
                            upd.execute(params![id, size, mtime, now])?;
                            stats.changed += 1;
                            changed_any = true;
                        } else if was_missing {
                            revive.execute(params![id, now])?;
                            stats.changed += 1;
                            changed_any = true;
                        } else {
                            stats.unchanged += 1;
                        }
                    }
                }
            }
            let mut gone = tx.prepare("UPDATE files SET missing=1, last_seen=?2 WHERE id=?1")?;
            for (id, _, _, was_missing) in existing.values() {
                if !was_missing {
                    gone.execute(params![id, now])?;
                    stats.removed += 1;
                    changed_any = true;
                }
            }
        }
        if changed_any {
            bump_generation(&tx)?;
        }
        tx.commit()?;
        Ok(())
    })?;
    Ok(stats)
}

// ------------------------------------------------------------------ phase 2

#[derive(Debug)]
pub struct Inspected {
    pub hash: String,
    pub width: u32,
    pub height: u32,
    pub taken_at: Option<i64>,
    pub format: String,
}

#[derive(Debug)]
pub enum InspectError {
    Unsupported(String),
    Corrupt(String),
}

pub fn inspect(path: &Path) -> Result<Inspected, InspectError> {
    use InspectError::*;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_heif = matches!(ext.as_str(), "heic" | "heif" | "hif");

    // Dimensions (cheap: header only).
    let (mut width, mut height, format, orientation_applied) = if is_heif {
        let (w, h) =
            crate::imaging::heif_dimensions(path).map_err(|e| Unsupported(format!("{e:#}")))?;
        (w, h, "heic".to_string(), true)
    } else {
        let reader = image::ImageReader::open(path)
            .and_then(|r| r.with_guessed_format())
            .map_err(|e| Corrupt(format!("open: {e}")))?;
        let fmt = reader.format();
        let Some(fmt) = fmt else {
            return Err(Unsupported("unrecognised image format".into()));
        };
        let (w, h) = reader
            .into_dimensions()
            .map_err(|e| Corrupt(format!("{e}")))?;
        (w, h, format!("{fmt:?}").to_ascii_lowercase(), false)
    };

    // EXIF: orientation (stored dimensions must be "as displayed") and capture date.
    let mut taken_at = None;
    if let Ok(f) = std::fs::File::open(path) {
        if let Ok(exif) = exif::Reader::new().read_from_container(&mut BufReader::new(f)) {
            if !orientation_applied {
                let o = exif
                    .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
                    .and_then(|f| f.value.get_uint(0));
                if matches!(o, Some(5..=8)) {
                    std::mem::swap(&mut width, &mut height);
                }
            }
            taken_at = exif
                .get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
                .and_then(|f| match &f.value {
                    exif::Value::Ascii(v) => {
                        v.first().and_then(|b| exif::DateTime::from_ascii(b).ok())
                    }
                    _ => None,
                })
                .and_then(|d| {
                    chrono::NaiveDate::from_ymd_opt(d.year as i32, d.month as u32, d.day as u32)
                        .and_then(|date| {
                            date.and_hms_opt(d.hour as u32, d.minute as u32, d.second as u32)
                        })
                        .map(|dt| dt.and_utc().timestamp())
                });
        }
    }
    if width == 0 || height == 0 {
        return Err(Corrupt("zero-sized image".into()));
    }

    // Content hash.
    let mut hasher = blake3::Hasher::new();
    let f = std::fs::File::open(path).map_err(|e| Corrupt(format!("open: {e}")))?;
    hasher
        .update_reader(f)
        .map_err(|e| Corrupt(format!("read: {e}")))?;
    Ok(Inspected {
        hash: hasher.finalize().to_hex().to_string(),
        width,
        height,
        taken_at,
        format,
    })
}

pub fn enrich_batch(db: &Db, cfg: &Config, state: &ScanState) -> Result<usize> {
    const BATCH: usize = 256;
    let rows: Vec<(i64, String)> = db.with(|c| {
        let mut st = c.prepare(
            "SELECT id, path FROM files WHERE status='pending' AND missing=0 ORDER BY id LIMIT ?1",
        )?;
        let v = st
            .query_map([BATCH as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    })?;
    if rows.is_empty() {
        return Ok(0);
    }

    let root = Path::new(&cfg.library.dir);
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(i64, Result<Inspected, InspectError>)>> =
        Mutex::new(Vec::with_capacity(rows.len()));
    let workers = cfg.scan.threads.clamp(1, 32).min(rows.len());
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Relaxed);
                let Some((id, rel)) = rows.get(i) else { break };
                let r = inspect(&root.join(rel));
                results.lock().unwrap().push((*id, r));
            });
        }
    });

    let results = results.into_inner().unwrap();
    let (mut ok, mut err) = (0u64, 0u64);
    db.with(|conn| {
        let tx = conn.transaction()?;
        {
            let mut good = tx.prepare(
                "UPDATE files SET hash=?2, width=?3, height=?4, taken_at=?5, format=?6, status='ok', error=NULL
                 WHERE id=?1 AND status='pending'",
            )?;
            let mut bad = tx.prepare("UPDATE files SET status=?2, error=?3 WHERE id=?1 AND status='pending'")?;
            for (id, r) in &results {
                match r {
                    Ok(m) => {
                        good.execute(params![id, m.hash, m.width, m.height, m.taken_at, m.format])?;
                        ok += 1;
                    }
                    Err(InspectError::Unsupported(e)) => {
                        debug!(id, "unsupported: {e}");
                        bad.execute(params![id, "unsupported", e])?;
                        err += 1;
                    }
                    Err(InspectError::Corrupt(e)) => {
                        debug!(id, "corrupt: {e}");
                        bad.execute(params![id, "corrupt", e])?;
                        err += 1;
                    }
                }
            }
        }
        bump_generation(&tx)?;
        tx.commit()?;
        Ok(())
    })?;
    state.enriched_ok_total.fetch_add(ok, Relaxed);
    state.enriched_err_total.fetch_add(err, Relaxed);
    Ok(rows.len())
}

// ------------------------------------------------------------------ change detection

/// inotify (via `notify`) change detection. Events are coalesced: after the first event we wait
/// `debounce`, drain whatever else arrived, then run one incremental scan.
pub fn spawn_watcher(scanner: Arc<Scanner>, root: String, debounce: Duration) -> Result<()> {
    use notify::{RecursiveMode, Watcher};
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(ev) if ev.kind.is_create() || ev.kind.is_modify() || ev.kind.is_remove() => {
                let _ = tx.send(());
            }
            Ok(_) => {}
            Err(e) => warn!("watch error: {e}"),
        })?;
    watcher.watch(Path::new(&root), RecursiveMode::Recursive)?;
    info!(%root, ?debounce, "inotify watcher enabled");
    tokio::spawn(async move {
        let _keep_alive = watcher;
        while rx.recv().await.is_some() {
            tokio::time::sleep(debounce).await;
            while rx.try_recv().is_ok() {}
            debug!("filesystem change detected, rescanning");
            scanner.trigger(false);
        }
    });
    Ok(())
}

pub fn spawn_periodic(scanner: Arc<Scanner>, every: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.tick().await; // first tick fires immediately; the startup scan covers it
        loop {
            tick.tick().await;
            scanner.trigger(false);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::fs;

    fn jpeg(path: &Path, w: u32, h: u32, shade: u8) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        image::RgbImage::from_pixel(w, h, image::Rgb([shade, 100, 50]))
            .save(path)
            .unwrap();
    }

    fn cfg_for(dir: &Path) -> Config {
        let mut c = Config::parse("").unwrap();
        c.library.dir = dir.to_str().unwrap().into();
        c.scan.threads = 2;
        c
    }

    fn scan_all(db: &Db, cfg: &Config, full: bool) -> WalkStats {
        let st = ScanState::default();
        let w = walk_and_upsert(db, cfg, full).unwrap();
        while enrich_batch(db, cfg, &st).unwrap() > 0 {}
        w
    }

    #[test]
    fn scan_detects_new_changed_removed_and_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        jpeg(&root.join("a.jpg"), 64, 48, 10);
        jpeg(&root.join("sub/b.jpg"), 48, 64, 20);
        fs::copy(root.join("a.jpg"), root.join("sub/a-copy.jpg")).unwrap();
        jpeg(&root.join("@eaDir/thumb.jpg"), 8, 8, 1); // must be ignored
        fs::write(root.join("notes.txt"), "x").unwrap();
        fs::write(root.join("broken.jpg"), "not a jpeg").unwrap();

        let cfg = cfg_for(root);
        let db = Db::open_memory().unwrap();
        let w = scan_all(&db, &cfg, false);
        assert_eq!((w.found, w.new), (4, 4));

        let s = db.with(|c| db::library_stats(c)).unwrap();
        assert_eq!(s.files_total, 4);
        assert_eq!(s.files_ok, 3);
        assert_eq!(
            s.unique_images, 2,
            "a.jpg and its byte-identical copy count once"
        );
        assert_eq!(s.duplicate_files, 1);
        assert_eq!(s.files_unsupported + s.files_corrupt, 1);
        assert_eq!((s.landscape, s.portrait), (2, 1));

        // Second scan with nothing changed does no work.
        let w = scan_all(&db, &cfg, false);
        assert_eq!((w.new, w.changed, w.removed, w.unchanged), (0, 0, 0, 4));

        // Modify, add and remove.
        std::thread::sleep(Duration::from_millis(20));
        jpeg(&root.join("a.jpg"), 80, 60, 99);
        jpeg(&root.join("c.jpg"), 32, 32, 5);
        fs::remove_file(root.join("sub/b.jpg")).unwrap();
        let w = scan_all(&db, &cfg, false);
        assert_eq!((w.new, w.changed, w.removed), (1, 1, 1));
        let s = db.with(|c| db::library_stats(c)).unwrap();
        assert_eq!(s.files_missing, 1);
        assert_eq!(s.unique_images, 3); // a (new content), a-copy (old content), c
        assert_eq!(s.duplicate_files, 0);
    }

    #[test]
    fn refuses_to_wipe_index_when_library_vanishes() {
        let tmp = tempfile::tempdir().unwrap();
        jpeg(&tmp.path().join("a.jpg"), 16, 16, 1);
        let cfg = cfg_for(tmp.path());
        let db = Db::open_memory().unwrap();
        scan_all(&db, &cfg, false);
        fs::remove_file(tmp.path().join("a.jpg")).unwrap();
        assert!(walk_and_upsert(&db, &cfg, false).is_err());
        let s = db.with(|c| db::library_stats(c)).unwrap();
        assert_eq!(s.files_ok, 1);
    }

    #[cfg(not(feature = "heic"))]
    #[test]
    fn heic_is_reported_unsupported_with_a_helpful_message_when_not_built_in() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("IMG_0001.HEIC");
        fs::write(&p, b"\0\0\0\x18ftypheic").unwrap();
        match inspect(&p) {
            Err(InspectError::Unsupported(msg)) => {
                assert!(
                    msg.contains("WITH_HEIC=1") && msg.contains("convert-heic.sh"),
                    "{msg}"
                )
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn exif_orientation_swaps_stored_dimensions() {
        // 2x1 JPEG with an EXIF Orientation=6 (rotate 90 CW) block spliced in after SOI.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("rot.jpg");
        let mut buf = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut buf)
            .encode(&[0u8; 6], 2, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        let exif: Vec<u8> = {
            let mut t = b"Exif\0\0MM\0\x2A\0\0\0\x08".to_vec();
            t.extend_from_slice(&[0, 1, 0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0, 0, 0, 0, 0]);
            t
        };
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&exif);
        out.extend_from_slice(&buf[2..]);
        fs::write(&p, out).unwrap();
        let m = inspect(&p).unwrap();
        assert_eq!((m.width, m.height), (1, 2));
    }
}
