//! Counters, the JSON stats report and the Prometheus text exposition.

use crate::config::Config;
use crate::db::{self, now_unix, ClientStats, Db, LibraryStats};
use crate::scan::ScanState;
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

pub const RENDER_BUCKETS: [f64; 8] = [0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0];

pub struct Metrics {
    pub started: Instant,
    requests: Mutex<BTreeMap<(String, u16), u64>>,
    pub bytes_served: AtomicU64,
    pub advances: AtomicU64,
    pub not_modified: AtomicU64,
    pub window_closed: AtomicU64,
    pub render_hits: AtomicU64,
    pub render_misses: AtomicU64,
    pub render_errors: AtomicU64,
    render_buckets: [AtomicU64; 8],
    render_count: AtomicU64,
    render_sum_us: AtomicU64,
    pub cache_bytes: AtomicU64,
    pub cache_files: AtomicU64,
    pub cache_evicted_total: AtomicU64,
}

impl Metrics {
    pub fn new() -> Metrics {
        Metrics {
            started: Instant::now(),
            requests: Mutex::new(BTreeMap::new()),
            bytes_served: AtomicU64::new(0),
            advances: AtomicU64::new(0),
            not_modified: AtomicU64::new(0),
            window_closed: AtomicU64::new(0),
            render_hits: AtomicU64::new(0),
            render_misses: AtomicU64::new(0),
            render_errors: AtomicU64::new(0),
            render_buckets: Default::default(),
            render_count: AtomicU64::new(0),
            render_sum_us: AtomicU64::new(0),
            cache_bytes: AtomicU64::new(0),
            cache_files: AtomicU64::new(0),
            cache_evicted_total: AtomicU64::new(0),
        }
    }

    pub fn count_request(&self, route: &str, status: u16) {
        *self
            .requests
            .lock()
            .unwrap()
            .entry((route.to_string(), status))
            .or_default() += 1;
    }

    pub fn observe_render(&self, secs: f64) {
        self.render_count.fetch_add(1, Relaxed);
        self.render_sum_us.fetch_add((secs * 1e6) as u64, Relaxed);
        for (i, b) in RENDER_BUCKETS.iter().enumerate() {
            if secs <= *b {
                self.render_buckets[i].fetch_add(1, Relaxed);
            }
        }
    }
}

#[derive(Serialize)]
pub struct ScanReport {
    pub running: bool,
    pub first_walk_done: bool,
    pub scans_total: u64,
    pub last_started: i64,
    pub last_finished: i64,
    pub last_walk_ms: u64,
    pub last_total_ms: u64,
    pub last_found: u64,
    pub last_new: u64,
    pub last_changed: u64,
    pub last_removed: u64,
    pub last_unchanged: u64,
    pub indexed_ok_total: u64,
    pub indexed_error_total: u64,
    pub last_error: Option<String>,
}

#[derive(Serialize)]
pub struct CacheReport {
    pub files: u64,
    pub bytes: u64,
    pub max_bytes: u64,
    pub hits: u64,
    pub misses: u64,
    pub errors: u64,
    pub evicted_total: u64,
    pub render_avg_ms: f64,
}

#[derive(Serialize)]
pub struct Report {
    pub version: &'static str,
    /// Whether this build can decode HEIC/HEIF (the official image cannot; see README).
    pub heic_support: bool,
    pub uptime_secs: u64,
    pub library: LibraryStats,
    pub scan: ScanReport,
    pub cache: CacheReport,
    pub clients: Vec<ClientStats>,
    pub requests: Vec<(String, u16, u64)>,
    pub images_advanced_total: u64,
    pub not_modified_total: u64,
    pub window_closed_total: u64,
    pub bytes_served_total: u64,
}

pub async fn build_report(db: &Db, cfg: &Config, scan: &ScanState, m: &Metrics) -> Result<Report> {
    let (library, clients) = db
        .run(|c| Ok((db::library_stats(c)?, db::client_stats(c)?)))
        .await?;
    let n = m.render_count.load(Relaxed);
    Ok(Report {
        version: env!("CARGO_PKG_VERSION"),
        heic_support: cfg!(feature = "heic"),
        uptime_secs: m.started.elapsed().as_secs(),
        library,
        scan: ScanReport {
            running: scan.running.load(Relaxed),
            first_walk_done: scan.first_walk_done.load(Relaxed),
            scans_total: scan.scans_total.load(Relaxed),
            last_started: scan.last_started.load(Relaxed),
            last_finished: scan.last_finished.load(Relaxed),
            last_walk_ms: scan.last_walk_ms.load(Relaxed),
            last_total_ms: scan.last_total_ms.load(Relaxed),
            last_found: scan.last_found.load(Relaxed),
            last_new: scan.last_new.load(Relaxed),
            last_changed: scan.last_changed.load(Relaxed),
            last_removed: scan.last_removed.load(Relaxed),
            last_unchanged: scan.last_unchanged.load(Relaxed),
            indexed_ok_total: scan.enriched_ok_total.load(Relaxed),
            indexed_error_total: scan.enriched_err_total.load(Relaxed),
            last_error: scan.last_error.lock().unwrap().clone(),
        },
        cache: CacheReport {
            files: m.cache_files.load(Relaxed),
            bytes: m.cache_bytes.load(Relaxed),
            max_bytes: cfg.storage.cache_max_mb * 1024 * 1024,
            hits: m.render_hits.load(Relaxed),
            misses: m.render_misses.load(Relaxed),
            errors: m.render_errors.load(Relaxed),
            evicted_total: m.cache_evicted_total.load(Relaxed),
            render_avg_ms: if n > 0 {
                m.render_sum_us.load(Relaxed) as f64 / n as f64 / 1000.0
            } else {
                0.0
            },
        },
        clients,
        requests: m
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|((r, s), c)| (r.clone(), *s, *c))
            .collect(),
        images_advanced_total: m.advances.load(Relaxed),
        not_modified_total: m.not_modified.load(Relaxed),
        window_closed_total: m.window_closed.load(Relaxed),
        bytes_served_total: m.bytes_served.load(Relaxed),
    })
}

fn metric(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(
        out,
        "# HELP frameshuffle_{name} {help}\n# TYPE frameshuffle_{name} {kind}"
    );
}

pub fn prometheus(r: &Report, m: &Metrics) -> String {
    let mut o = String::with_capacity(4096);
    macro_rules! simple {
        ($name:expr, $kind:expr, $help:expr, $val:expr) => {{
            metric(&mut o, $name, $kind, $help);
            let _ = writeln!(o, "frameshuffle_{} {}", $name, $val);
        }};
    }
    let l = &r.library;
    metric(&mut o, "build_info", "gauge", "Version information");
    let _ = writeln!(o, "frameshuffle_build_info{{version=\"{}\"}} 1", r.version);
    simple!(
        "heic_support",
        "gauge",
        "1 if this build can decode HEIC/HEIF",
        r.heic_support as u8
    );
    simple!(
        "uptime_seconds",
        "gauge",
        "Seconds since start",
        r.uptime_secs
    );

    metric(
        &mut o,
        "library_files",
        "gauge",
        "Indexed files by status (excludes files no longer on disk)",
    );
    for (s, v) in [
        ("ok", l.files_ok),
        ("pending", l.files_pending),
        ("unsupported", l.files_unsupported),
        ("corrupt", l.files_corrupt),
    ] {
        let _ = writeln!(o, "frameshuffle_library_files{{status=\"{s}\"}} {v}");
    }
    simple!(
        "library_files_total",
        "gauge",
        "Files currently on disk that match the library filters",
        l.files_total
    );
    simple!(
        "library_files_missing",
        "gauge",
        "Previously indexed files that have disappeared",
        l.files_missing
    );
    simple!(
        "library_unique_images",
        "gauge",
        "Distinct images (by content hash) eligible for display",
        l.unique_images
    );
    simple!(
        "library_duplicate_files",
        "gauge",
        "Exact-duplicate files suppressed from display",
        l.duplicate_files
    );
    simple!(
        "library_bytes",
        "gauge",
        "Total size of files on disk",
        l.total_bytes
    );
    metric(
        &mut o,
        "library_orientation",
        "gauge",
        "Eligible files by orientation",
    );
    for (s, v) in [
        ("landscape", l.landscape),
        ("portrait", l.portrait),
        ("square", l.square),
    ] {
        let _ = writeln!(
            o,
            "frameshuffle_library_orientation{{orientation=\"{s}\"}} {v}"
        );
    }
    metric(
        &mut o,
        "library_format",
        "gauge",
        "Eligible files by format",
    );
    for (f, v) in &l.by_format {
        let _ = writeln!(o, "frameshuffle_library_format{{format=\"{f}\"}} {v}");
    }
    simple!(
        "index_generation",
        "counter",
        "Increments whenever the set of eligible files changes",
        l.generation
    );
    simple!(
        "database_bytes",
        "gauge",
        "SQLite database size",
        l.db_bytes
    );

    let s = &r.scan;
    simple!(
        "scan_running",
        "gauge",
        "1 while a scan is in progress",
        s.running as u8
    );
    simple!("scans_total", "counter", "Completed scans", s.scans_total);
    simple!(
        "scan_last_duration_seconds",
        "gauge",
        "Duration of the last completed scan",
        s.last_total_ms as f64 / 1000.0
    );
    simple!(
        "scan_last_walk_seconds",
        "gauge",
        "Directory walk portion of the last scan",
        s.last_walk_ms as f64 / 1000.0
    );
    simple!(
        "scan_last_finished_timestamp_seconds",
        "gauge",
        "Unix time the last scan finished",
        s.last_finished
    );
    simple!(
        "scan_last_files_found",
        "gauge",
        "Files found by the last walk",
        s.last_found
    );
    metric(
        &mut o,
        "scan_last_changes",
        "gauge",
        "Changes detected by the last walk",
    );
    for (k, v) in [
        ("new", s.last_new),
        ("changed", s.last_changed),
        ("removed", s.last_removed),
        ("unchanged", s.last_unchanged),
    ] {
        let _ = writeln!(o, "frameshuffle_scan_last_changes{{kind=\"{k}\"}} {v}");
    }
    metric(
        &mut o,
        "indexed_files_total",
        "counter",
        "Files hashed and inspected since start",
    );
    let _ = writeln!(
        o,
        "frameshuffle_indexed_files_total{{result=\"ok\"}} {}",
        s.indexed_ok_total
    );
    let _ = writeln!(
        o,
        "frameshuffle_indexed_files_total{{result=\"error\"}} {}",
        s.indexed_error_total
    );

    let c = &r.cache;
    simple!(
        "cache_files",
        "gauge",
        "Converted images in the cache",
        c.files
    );
    simple!("cache_bytes", "gauge", "Cache size on disk", c.bytes);
    simple!(
        "cache_max_bytes",
        "gauge",
        "Configured cache cap",
        c.max_bytes
    );
    simple!(
        "cache_evicted_total",
        "counter",
        "Cache files evicted for size",
        c.evicted_total
    );
    metric(
        &mut o,
        "render_cache_total",
        "counter",
        "Conversion cache lookups",
    );
    let _ = writeln!(
        o,
        "frameshuffle_render_cache_total{{result=\"hit\"}} {}",
        c.hits
    );
    let _ = writeln!(
        o,
        "frameshuffle_render_cache_total{{result=\"miss\"}} {}",
        c.misses
    );
    simple!(
        "render_errors_total",
        "counter",
        "Failed conversions",
        c.errors
    );

    metric(
        &mut o,
        "render_duration_seconds",
        "histogram",
        "Time to convert one image (cache misses only)",
    );
    for (i, b) in RENDER_BUCKETS.iter().enumerate() {
        let _ = writeln!(
            o,
            "frameshuffle_render_duration_seconds_bucket{{le=\"{b}\"}} {}",
            m.render_buckets[i].load(Relaxed)
        );
    }
    let n = m.render_count.load(Relaxed);
    let _ = writeln!(
        o,
        "frameshuffle_render_duration_seconds_bucket{{le=\"+Inf\"}} {n}"
    );
    let _ = writeln!(
        o,
        "frameshuffle_render_duration_seconds_sum {}",
        m.render_sum_us.load(Relaxed) as f64 / 1e6
    );
    let _ = writeln!(o, "frameshuffle_render_duration_seconds_count {n}");

    simple!(
        "images_advanced_total",
        "counter",
        "Times a client was moved on to a new image",
        r.images_advanced_total
    );
    simple!(
        "not_modified_total",
        "counter",
        "304 responses (frame skipped a refresh)",
        r.not_modified_total
    );
    simple!(
        "window_closed_responses_total",
        "counter",
        "Requests answered while outside the client's time window",
        r.window_closed_total
    );
    simple!(
        "bytes_served_total",
        "counter",
        "Image bytes sent",
        r.bytes_served_total
    );

    metric(
        &mut o,
        "http_requests_total",
        "counter",
        "HTTP requests by route and status",
    );
    for (route, status, n) in &r.requests {
        let _ = writeln!(
            o,
            "frameshuffle_http_requests_total{{route=\"{route}\",status=\"{status}\"}} {n}"
        );
    }

    let now = now_unix();
    for (name, kind, help, f) in [
        (
            "client_deck_size",
            "gauge",
            "Images in the client's current shuffle",
            (|c: &ClientStats| c.deck_size) as fn(&ClientStats) -> i64,
        ),
        (
            "client_deck_remaining",
            "gauge",
            "Images not yet shown in the client's current cycle",
            |c| c.deck_remaining,
        ),
        (
            "client_cycle",
            "gauge",
            "Completed-cycle counter (1 = first pass through the library)",
            |c| c.cycle,
        ),
        (
            "client_served_total",
            "counter",
            "Images served to the client",
            |c| c.served_total,
        ),
    ] {
        metric(&mut o, name, kind, help);
        for c in &r.clients {
            let _ = writeln!(o, "frameshuffle_{name}{{client=\"{}\"}} {}", c.name, f(c));
        }
    }
    metric(
        &mut o,
        "client_last_seen_age_seconds",
        "gauge",
        "Seconds since the client last made a request",
    );
    for c in &r.clients {
        let _ = writeln!(
            o,
            "frameshuffle_client_last_seen_age_seconds{{client=\"{}\"}} {}",
            c.name,
            (now - c.last_seen).max(0)
        );
    }
    o
}
