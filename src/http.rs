//! HTTP API.
//!
//!   GET  /image?client=NAME[&profile=P]   the client's image. Stays the same until the client's interval
//!                                         elapses, then moves to the next card in its shuffled deck.
//!                                         (/random is an alias, for drop-in use with the old Flask app.)
//!   GET  /current?client=NAME             the current image; never advances (unless nothing was served yet)
//!   POST /skip?client=NAME                advance now
//!   GET  /img/{id}?profile=P              a specific image (full hash or 16+ char prefix), immutably cacheable
//!   GET  /health  /ready  /stats  /metrics
//!   POST /admin/rescan[?full=1]   POST /admin/reset?client=NAME

use crate::cache;
use crate::config::{valid_client_name, Config, Profile};
use crate::db::{self, Db, SelectMode};
use crate::imaging;
use crate::metrics::{self, Metrics};
use crate::scan::{Scanner, Trigger};
use crate::schedule::{State as WinState, Windows};
use anyhow::anyhow;
use axum::body::Body;
use axum::extract::{MatchedPath, Path as UrlPath, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub scanner: Arc<Scanner>,
    pub metrics: Arc<Metrics>,
    windows: HashMap<String, Windows>,
    default_windows: Windows,
    fingerprints: HashMap<String, String>,
    render_sem: Semaphore,
    inflight: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

impl AppState {
    pub fn new(
        cfg: Arc<Config>,
        db: Db,
        scanner: Arc<Scanner>,
        metrics: Arc<Metrics>,
    ) -> anyhow::Result<Arc<AppState>> {
        let mk = |c: &crate::config::ClientCfg| {
            Windows::new(
                c.timezone.as_deref().unwrap_or(&cfg.server.timezone),
                &c.windows,
            )
        };
        let mut windows = HashMap::new();
        for (name, c) in &cfg.clients {
            windows.insert(name.clone(), mk(c)?);
        }
        let default_windows = mk(&cfg.default_client)?;
        let fingerprints: HashMap<String, String> = cfg
            .profiles
            .iter()
            .map(|(n, p)| (n.clone(), cache::fingerprint(p)))
            .collect();
        for (n, fp) in &fingerprints {
            cache::ensure_marker(&cache::profile_dir(&cfg.storage.cache_dir, n, fp));
        }
        let permits = cfg.server.render_concurrency.max(1);
        Ok(Arc::new(AppState {
            cfg,
            db,
            scanner,
            metrics,
            windows,
            default_windows,
            fingerprints,
            render_sem: Semaphore::new(permits),
            inflight: Mutex::new(HashMap::new()),
        }))
    }

    /// (cache directory name, fingerprint) for every cache directory that is currently valid.
    pub fn fingerprints(&self) -> Vec<(String, String)> {
        let mut v: Vec<_> = self
            .fingerprints
            .iter()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect();
        v.push((
            "original-heic".to_string(),
            format!("q{}", self.cfg.processing.heic_quality),
        ));
        v
    }

    fn windows_for(&self, client: &str) -> &Windows {
        self.windows.get(client).unwrap_or(&self.default_windows)
    }

    fn authorize(
        &self,
        headers: &HeaderMap,
        query_token: Option<&str>,
        admin: bool,
    ) -> Result<(), AppError> {
        let required = if admin {
            self.cfg.server.admin_token.as_deref()
        } else {
            self.cfg.server.token.as_deref()
        };
        let Some(required) = required else {
            return Ok(());
        };
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        // Admin token also opens the client endpoints.
        let ok = [query_token, bearer].into_iter().flatten().any(|t| {
            ct_eq(t, required)
                || (!admin
                    && self
                        .cfg
                        .server
                        .admin_token
                        .as_deref()
                        .is_some_and(|a| ct_eq(t, a)))
        });
        if ok {
            Ok(())
        } else {
            Err(AppError::new(
                StatusCode::UNAUTHORIZED,
                "missing or invalid token",
            ))
        }
    }

    /// Identifies *how* `src` is delivered, so a config change invalidates ETags held by frames.
    fn variant_tag(&self, src: &Path, pname: &str) -> String {
        let p = &self.cfg.processing;
        if p.enabled {
            self.fingerprints.get(pname).cloned().unwrap_or_default()
        } else if p.convert_heic && imaging::is_heif(src) {
            format!("heic-q{}", p.heic_quality)
        } else {
            "orig".to_string()
        }
    }

    /// The bytes and content type to send for `src`: converted to a profile (processing enabled),
    /// HEIC transcoded to full-size JPEG, or the original file untouched.
    async fn produce(
        &self,
        hash: &str,
        src: &Path,
        pname: &str,
        profile: Option<&Profile>,
    ) -> anyhow::Result<(Vec<u8>, &'static str)> {
        let p = &self.cfg.processing;
        if p.enabled {
            let profile = profile.ok_or_else(|| anyhow!("no profile"))?.clone();
            let fp = self
                .fingerprints
                .get(pname)
                .ok_or_else(|| anyhow!("unknown profile"))?
                .clone();
            let (ct, ext) = (
                imaging::content_type(&profile),
                imaging::extension(&profile),
            );
            let bytes = self
                .render_cached(hash, src, pname, &fp, ext, move |s| {
                    imaging::render(s, &profile)
                })
                .await?;
            Ok((bytes, ct))
        } else if p.convert_heic && imaging::is_heif(src) {
            let q = p.heic_quality;
            let bytes = self
                .render_cached(
                    hash,
                    src,
                    "original-heic",
                    &format!("q{q}"),
                    "jpg",
                    move |s| imaging::transcode_jpeg(s, q),
                )
                .await?;
            Ok((bytes, "image/jpeg"))
        } else {
            Ok((
                tokio::fs::read(src).await?,
                imaging::content_type_for_path(src),
            ))
        }
    }

    /// Return a converted image, from the on-disk cache when possible. `convert` runs on a blocking thread.
    async fn render_cached<F>(
        &self,
        hash: &str,
        src: &Path,
        cache_name: &str,
        fp: &str,
        ext: &str,
        convert: F,
    ) -> anyhow::Result<Vec<u8>>
    where
        F: FnOnce(&Path) -> anyhow::Result<Vec<u8>> + Send + 'static,
    {
        let path = cache::file_path(&self.cfg.storage.cache_dir, cache_name, fp, hash, ext);

        let try_cache = |p: PathBuf| async move {
            tokio::task::spawn_blocking(move || cache::read_and_touch(&p))
                .await
                .ok()
                .flatten()
        };
        if let Some(b) = try_cache(path.clone()).await {
            self.metrics.render_hits.fetch_add(1, Relaxed);
            return Ok(b);
        }
        // Single-flight: concurrent requests for the same rendering wait for the first one.
        let gate = {
            let mut m = self.inflight.lock().unwrap();
            m.entry(path.clone()).or_default().clone()
        };
        let _g = gate.lock().await;
        let result = async {
            if let Some(b) = try_cache(path.clone()).await {
                self.metrics.render_hits.fetch_add(1, Relaxed);
                return Ok(b);
            }
            let _permit = self.render_sem.acquire().await?;
            self.metrics.render_misses.fetch_add(1, Relaxed);
            let t = Instant::now();
            let s = src.to_path_buf();
            let bytes = match tokio::task::spawn_blocking(move || convert(&s)).await? {
                Ok(b) => b,
                Err(e) => {
                    self.metrics.render_errors.fetch_add(1, Relaxed);
                    return Err(e);
                }
            };
            self.metrics.observe_render(t.elapsed().as_secs_f64());
            debug!(
                ms = t.elapsed().as_millis() as u64,
                bytes = bytes.len(),
                "converted {}",
                &hash[..12]
            );
            let (p2, b2) = (path.clone(), bytes.clone());
            let dir = cache::profile_dir(&self.cfg.storage.cache_dir, cache_name, fp);
            match tokio::task::spawn_blocking(move || {
                cache::ensure_marker(&dir);
                cache::write_atomic(&p2, &b2)
            })
            .await
            {
                Ok(Ok(())) => {
                    self.metrics
                        .cache_bytes
                        .fetch_add(bytes.len() as u64, Relaxed);
                    self.metrics.cache_files.fetch_add(1, Relaxed);
                }
                Ok(Err(e)) => warn!("cache write failed: {e}"),
                Err(e) => warn!("cache write task failed: {e}"),
            }
            Ok(bytes)
        }
        .await;
        self.inflight.lock().unwrap().remove(&path);
        result
    }
}

fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

// ------------------------------------------------------------------ errors

pub struct AppError {
    status: StatusCode,
    msg: String,
    retry_after: Option<u64>,
}

impl AppError {
    fn new(status: StatusCode, msg: impl Into<String>) -> Self {
        AppError {
            status,
            msg: msg.into(),
            retry_after: None,
        }
    }
    fn retry(mut self, secs: u64) -> Self {
        self.retry_after = Some(secs);
        self
    }
}
impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        warn!("internal error: {e:#}");
        AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let mut r = (self.status, format!("{}\n", self.msg)).into_response();
        if let Some(s) = self.retry_after {
            r.headers_mut()
                .insert(HN_NEXT_REFRESH, HeaderValue::from(s));
        }
        r
    }
}

// ------------------------------------------------------------------ handlers

const HN_NEXT_REFRESH: HeaderName = HeaderName::from_static("x-next-refresh");

#[derive(Deserialize, Default)]
pub struct ImgQuery {
    client: Option<String>,
    profile: Option<String>,
    token: Option<String>,
}

fn hv(s: &str) -> HeaderValue {
    HeaderValue::from_str(s).unwrap_or_else(|_| HeaderValue::from_static(""))
}

async fn image(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<ImgQuery>,
) -> Result<Response, AppError> {
    serve(&s, &h, &q, SelectMode::Auto).await
}
async fn current(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<ImgQuery>,
) -> Result<Response, AppError> {
    serve(&s, &h, &q, SelectMode::Peek).await
}
async fn skip(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<ImgQuery>,
) -> Result<Response, AppError> {
    serve(&s, &h, &q, SelectMode::Force).await
}

async fn serve(
    s: &Arc<AppState>,
    headers: &HeaderMap,
    q: &ImgQuery,
    mut mode: SelectMode,
) -> Result<Response, AppError> {
    s.authorize(headers, q.token.as_deref(), false)?;
    let client = q.client.clone().unwrap_or_else(|| "default".into());
    if !valid_client_name(&client) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "invalid client name (use letters, digits, - _ .)",
        ));
    }
    let ccfg = s.cfg.client(&client).clone();
    let pname = q.profile.clone().unwrap_or_else(|| ccfg.profile.clone());
    // Profiles only matter when processing is enabled; otherwise originals are served and ?profile= is ignored.
    let profile = if s.cfg.processing.enabled {
        Some(s.cfg.profiles.get(&pname).ok_or_else(|| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                format!("unknown profile '{pname}'"),
            )
        })?)
    } else {
        None
    };

    let now: DateTime<Utc> = Utc::now();
    let now_ts = now.timestamp();
    let windows = s.windows_for(&client);
    let win = windows.state_at(now);

    if let WinState::Closed { opens_in } = win {
        if mode == SelectMode::Auto {
            s.metrics.window_closed.fetch_add(1, Relaxed);
            if ccfg.outside_window == crate::config::OutsideWindow::NoContent {
                let mut r = StatusCode::NO_CONTENT.into_response();
                r.headers_mut()
                    .insert(HN_NEXT_REFRESH, HeaderValue::from(opens_in));
                r.headers_mut().insert("x-window", hv("closed"));
                return Ok(r);
            }
            mode = SelectMode::Peek; // keep showing what is already on the screen
        }
    }

    let root = Path::new(&s.cfg.library.dir);
    let mut attempts = 0;
    let (sel, bytes, content_type, etag) = loop {
        attempts += 1;
        if attempts > 5 {
            return Err(AppError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "could not find a usable image",
            )
            .retry(60));
        }
        let (cl, cc) = (client.clone(), ccfg.clone());
        let sel =
            s.db.run(move |c| db::select_image(c, &cl, &cc, mode, now_ts))
                .await?;
        let Some(sel) = sel else {
            return Err(AppError::new(
                StatusCode::NOT_FOUND,
                "no images available for this client",
            )
            .retry(300));
        };
        let full = root.join(&sel.rel_path);

        // 304: the frame already shows this exact rendering, so it can skip a (slow, battery-hungry) e-paper refresh.
        let etag = format!("\"{}-{}\"", &sel.hash[..16], s.variant_tag(&full, &pname));
        if headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            == Some(etag.as_str())
            && tokio::fs::metadata(&full).await.is_ok()
        {
            s.metrics.not_modified.fetch_add(1, Relaxed);
            let mut r = StatusCode::NOT_MODIFIED.into_response();
            decorate(&mut r, s, &client, &ccfg, windows, &sel, &etag, now, win);
            return Ok(r);
        }

        if !tokio::fs::metadata(&full).await.is_ok_and(|m| m.is_file()) {
            warn!("{} vanished from disk; marking missing", sel.rel_path);
            let p = sel.rel_path.clone();
            s.db.run(move |c| db::mark_missing(c, &p)).await?;
            mode = SelectMode::Force;
            continue;
        }
        match s.produce(&sel.hash, &full, &pname, profile).await {
            Ok((b, ct)) => break (sel, b, ct, etag),
            Err(e) => {
                warn!("cannot convert {}: {e:#}", sel.rel_path);
                let (p, msg) = (sel.rel_path.clone(), format!("{e:#}"));
                s.db.run(move |c| {
                    c.execute(
                        "UPDATE files SET status='corrupt', error=?2 WHERE path=?1",
                        rusqlite::params![p, msg],
                    )?;
                    db::bump_generation(c)
                })
                .await?;
                mode = SelectMode::Force;
            }
        }
    };

    if sel.advanced {
        s.metrics.advances.fetch_add(1, Relaxed);
        info!(
            client,
            id = &sel.hash[..12],
            cycle = sel.cycle,
            remaining = sel.deck_remaining,
            "serving new image"
        );
    }
    s.metrics
        .bytes_served
        .fetch_add(bytes.len() as u64, Relaxed);
    let mut r = Response::new(Body::from(bytes));
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    decorate(&mut r, s, &client, &ccfg, windows, &sel, &etag, now, win);
    Ok(r)
}

#[allow(clippy::too_many_arguments)]
fn decorate(
    r: &mut Response,
    s: &AppState,
    client: &str,
    ccfg: &crate::config::ClientCfg,
    windows: &Windows,
    sel: &db::Selected,
    etag: &str,
    now: DateTime<Utc>,
    win: WinState,
) {
    let _ = s;
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::ETAG, hv(etag));
    h.insert("x-client", hv(client));
    h.insert("x-image-id", hv(&sel.hash[..16]));
    h.insert("x-cycle", HeaderValue::from(sel.cycle));
    h.insert("x-deck-size", HeaderValue::from(sel.deck_size));
    h.insert("x-deck-remaining", HeaderValue::from(sel.deck_remaining));
    h.insert("x-advanced", hv(if sel.advanced { "1" } else { "0" }));
    h.insert(
        "x-window",
        hv(if matches!(win, WinState::Open) {
            "open"
        } else {
            "closed"
        }),
    );

    // When should the frame come back? When the sticky interval ends, pushed forward to the next
    // window opening if that moment falls outside the client's window.
    let elapsed = (now.timestamp() - sel.since).max(0) as u64;
    let wait = ccfg.interval_secs.saturating_sub(elapsed);
    let secs = match win {
        WinState::Closed { opens_in } => windows
            .next_refresh(now, wait)
            .max(opens_in.min(wait.max(1))),
        WinState::Open if ccfg.interval_secs == 0 && windows.is_unrestricted() => 0,
        WinState::Open => windows.next_refresh(now, wait.max(1)),
    };
    if secs > 0 {
        h.insert(HN_NEXT_REFRESH, HeaderValue::from(secs));
    }
}

async fn image_by_id(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<ImgQuery>,
) -> Result<Response, AppError> {
    s.authorize(&h, q.token.as_deref(), false)?;
    if id.len() < 16 || id.len() > 64 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "id must be 16-64 hex characters",
        ));
    }
    let pname = q
        .profile
        .clone()
        .unwrap_or_else(|| s.cfg.default_client.profile.clone());
    let profile = if s.cfg.processing.enabled {
        Some(s.cfg.profiles.get(&pname).ok_or_else(|| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                format!("unknown profile '{pname}'"),
            )
        })?)
    } else {
        None
    };
    let pat = format!("{}%", id.to_ascii_lowercase());
    let found: Option<(String, String)> = s
        .db
        .run(move |c| {
            use rusqlite::OptionalExtension;
            Ok(c.query_row(
                "SELECT hash, path FROM files WHERE hash LIKE ?1 AND status='ok' AND missing=0 ORDER BY id LIMIT 1",
                [pat],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
        .await?;
    let Some((hash, rel)) = found else {
        return Err(AppError::new(StatusCode::NOT_FOUND, "unknown image"));
    };
    let full = Path::new(&s.cfg.library.dir).join(rel);
    let etag = format!("\"{}-{}\"", &hash[..16], s.variant_tag(&full, &pname));
    if h.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        let mut r = StatusCode::NOT_MODIFIED.into_response();
        r.headers_mut().insert(header::ETAG, hv(&etag));
        return Ok(r);
    }
    let (bytes, content_type) = s
        .produce(&hash, &full, &pname, profile)
        .await
        .map_err(|e| {
            warn!("cannot convert {}: {e:#}", &hash[..12]);
            AppError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "cannot convert this image",
            )
        })?;
    let mut r = Response::new(Body::from(bytes));
    let hd = r.headers_mut();
    hd.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    hd.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    hd.insert(header::ETAG, hv(&etag));
    Ok(r)
}

async fn health() -> &'static str {
    "ok\n"
}

async fn ready(State(s): State<Arc<AppState>>) -> Response {
    let walked = s.scanner.state.first_walk_done.load(Relaxed);
    let any = s.db.run(|c| db::has_eligible(c)).await.unwrap_or(false);
    let body = serde_json::json!({ "ready": walked && any, "first_scan_walk_done": walked, "has_images": any });
    let code = if walked && any {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(body)).into_response()
}

async fn stats(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<ImgQuery>,
) -> Result<Response, AppError> {
    s.authorize(&h, q.token.as_deref(), true)?;
    let r = metrics::build_report(&s.db, &s.cfg, &s.scanner.state, &s.metrics).await?;
    Ok(Json(r).into_response())
}

async fn prom(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<ImgQuery>,
) -> Result<Response, AppError> {
    s.authorize(&h, q.token.as_deref(), true)?;
    let r = metrics::build_report(&s.db, &s.cfg, &s.scanner.state, &s.metrics).await?;
    let mut resp = metrics::prometheus(&r, &s.metrics).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    Ok(resp)
}

#[derive(Deserialize, Default)]
struct AdminQuery {
    client: Option<String>,
    full: Option<String>,
    token: Option<String>,
}

async fn rescan(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<AdminQuery>,
) -> Result<Response, AppError> {
    s.authorize(&h, q.token.as_deref(), true)?;
    let full = matches!(q.full.as_deref(), Some("1" | "true" | "yes"));
    let t = s.scanner.trigger(full);
    let status = if t == Trigger::Started {
        "started"
    } else {
        "queued (a scan is already running)"
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": status, "full": full })),
    )
        .into_response())
}

async fn reset(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<AdminQuery>,
) -> Result<Response, AppError> {
    s.authorize(&h, q.token.as_deref(), true)?;
    let client = q.client.unwrap_or_default();
    if !valid_client_name(&client) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "client= is required",
        ));
    }
    let c2 = client.clone();
    let existed = s.db.run(move |c| db::reset_client(c, &c2)).await?;
    if !existed {
        return Err(AppError::new(StatusCode::NOT_FOUND, "unknown client"));
    }
    Ok(Json(
        serde_json::json!({ "client": client, "status": "deck reset; next request reshuffles" }),
    )
    .into_response())
}

async fn index() -> &'static str {
    "frameshuffle\n\n\
     GET  /image?client=NAME[&profile=P]   next image for a client (sticky until its interval elapses)\n\
     GET  /current?client=NAME             current image, never advances\n\
     POST /skip?client=NAME                advance now\n\
     GET  /img/{id}[?profile=P]            a specific image\n\
     GET  /health /ready /stats /metrics\n\
     POST /admin/rescan[?full=1]  POST /admin/reset?client=NAME\n"
}

/// Counts requests per matched route and logs one line each.
async fn track(State(s): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", |m| m.as_str())
        .to_string();
    let method = req.method().clone();
    let t = Instant::now();
    let resp = next.run(req).await;
    let status = resp.status().as_u16();
    s.metrics.count_request(&route, status);
    if route != "/health" && route != "/metrics" {
        debug!(
            "{method} {route} -> {status} in {}ms",
            t.elapsed().as_millis()
        );
    }
    resp
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/image", get(image))
        .route("/random", get(image))
        .route("/current", get(current))
        .route("/skip", post(skip).get(skip))
        .route("/img/{id}", get(image_by_id))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/stats", get(stats))
        .route("/metrics", get(prom))
        .route("/admin/rescan", post(rescan))
        .route("/admin/reset", post(reset))
        .route_layer(middleware::from_fn_with_state(state.clone(), track))
        .with_state(state)
}
