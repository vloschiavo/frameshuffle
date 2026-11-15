//! On-disk cache of converted images, keyed by (profile fingerprint, content hash).
//! A profile edit changes its fingerprint, so stale renderings are never served; the janitor
//! removes orphaned profile directories and evicts the oldest files when over the size cap.

use crate::config::{Config, Profile};
use crate::metrics::Metrics;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing::{info, warn};

const MARKER: &str = ".frameshuffle-cache";

pub fn fingerprint(p: &Profile) -> String {
    let json = serde_json::to_vec(p).unwrap_or_default();
    blake3::hash(&json).to_hex()[..10].to_string()
}

pub fn profile_dir(root: &str, name: &str, fp: &str) -> PathBuf {
    Path::new(root).join(format!("{name}-{fp}"))
}

pub fn file_path(root: &str, name: &str, fp: &str, hash: &str, ext: &str) -> PathBuf {
    profile_dir(root, name, fp)
        .join(&hash[..2])
        .join(format!("{hash}.{ext}"))
}

pub fn ensure_marker(dir: &Path) {
    let m = dir.join(MARKER);
    if !m.exists() {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(m, b"managed by frameshuffle\n");
    }
}

/// Read a cached rendering and bump its mtime so eviction approximates LRU.
pub fn read_and_touch(path: &Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
    Some(bytes)
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

struct Entry {
    path: PathBuf,
    size: u64,
    mtime: SystemTime,
}

fn sweep(cfg: &Config, fps: &[(String, String)], m: &Metrics) {
    let root = Path::new(&cfg.storage.cache_dir);
    let valid: HashSet<String> = fps.iter().map(|(n, f)| format!("{n}-{f}")).collect();
    let mut entries: Vec<Entry> = Vec::new();

    if let Ok(rd) = std::fs::read_dir(root) {
        for d in rd.flatten() {
            let path = d.path();
            if !path.is_dir() || !path.join(MARKER).exists() {
                continue; // never touch anything we didn't create
            }
            let name = d.file_name().to_string_lossy().to_string();
            if !valid.contains(&name) {
                info!("removing orphaned cache directory {name}");
                let _ = std::fs::remove_dir_all(&path);
                continue;
            }
            for sub in std::fs::read_dir(&path).into_iter().flatten().flatten() {
                for f in std::fs::read_dir(sub.path())
                    .into_iter()
                    .flatten()
                    .flatten()
                {
                    if let Ok(md) = f.metadata() {
                        if md.is_file() {
                            entries.push(Entry {
                                path: f.path(),
                                size: md.len(),
                                mtime: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                            });
                        }
                    }
                }
            }
        }
    }

    let max = cfg.storage.cache_max_mb * 1024 * 1024;
    let mut total: u64 = entries.iter().map(|e| e.size).sum();
    if total > max {
        entries.sort_by_key(|e| e.mtime);
        let target = max / 10 * 9;
        let mut evicted = 0u64;
        let mut keep_from = 0;
        for (i, e) in entries.iter().enumerate() {
            if total <= target {
                break;
            }
            if std::fs::remove_file(&e.path).is_ok() {
                total -= e.size;
                evicted += 1;
            }
            keep_from = i + 1;
        }
        m.cache_evicted_total.fetch_add(evicted, Relaxed);
        entries.drain(..keep_from);
        info!(evicted, "cache eviction");
    }
    m.cache_bytes.store(total, Relaxed);
    m.cache_files.store(entries.len() as u64, Relaxed);
}

pub fn spawn_janitor(cfg: Arc<Config>, fps: Vec<(String, String)>, m: Arc<Metrics>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(600));
        loop {
            tick.tick().await;
            let (c, f, mm) = (cfg.clone(), fps.clone(), m.clone());
            if let Err(e) = tokio::task::spawn_blocking(move || sweep(&c, &f, &mm)).await {
                warn!("cache janitor panicked: {e}");
            }
        }
    });
}
