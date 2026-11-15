//! SQLite storage: file index, per-client shuffle decks, history, stats.
//!
//! Deck design: one row per (client, content hash). `played=0` rows are the remaining "cards"
//! ordered by `ord`; picking the next image is "take the lowest `ord` where played=0". When none
//! remain the deck is reshuffled and the cycle counter increments. New files are inserted at a
//! uniformly random position among the *remaining* cards (ord drawn from (floor, 1) where `floor`
//! is the ord of the last card dealt). Keying by content hash gives exact-duplicate suppression
//! and keeps history across renames for free.

use crate::config::{ClientCfg, Filters, Orientation};
use anyhow::{Context, Result};
use rusqlite::{
    params, params_from_iter, types::Value, Connection, OptionalExtension, TransactionBehavior,
};
use serde::Serialize;
use std::sync::{Arc, Mutex};

const SCHEMA_V1: &str = r#"
CREATE TABLE files(
    id         INTEGER PRIMARY KEY,
    path       TEXT NOT NULL UNIQUE,   -- relative to the library root, '/' separated
    size       INTEGER NOT NULL,
    mtime_ns   INTEGER NOT NULL,
    hash       TEXT,                   -- BLAKE3 of the file bytes; NULL until processed
    phash      INTEGER,                -- reserved for a 64-bit perceptual hash (near-duplicate detection)
    width      INTEGER,                -- as displayed, i.e. after EXIF orientation
    height     INTEGER,
    taken_at   INTEGER,                -- EXIF DateTimeOriginal as unix seconds (wall-clock read as UTC)
    format     TEXT,
    status     TEXT NOT NULL DEFAULT 'pending',  -- pending | ok | unsupported | corrupt
    error      TEXT,
    missing    INTEGER NOT NULL DEFAULT 0,
    first_seen INTEGER NOT NULL,
    last_seen  INTEGER NOT NULL
);
CREATE INDEX files_hash   ON files(hash) WHERE hash IS NOT NULL;
CREATE INDEX files_status ON files(status, missing);
CREATE INDEX files_phash  ON files(phash) WHERE phash IS NOT NULL;

CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE clients(
    name              TEXT PRIMARY KEY,
    created_at        INTEGER NOT NULL,
    last_seen         INTEGER NOT NULL,
    cycle             INTEGER NOT NULL DEFAULT 1,
    deck_floor        REAL NOT NULL DEFAULT 0,
    current_hash      TEXT,
    current_since     INTEGER,
    served_total      INTEGER NOT NULL DEFAULT 0,
    synced_generation INTEGER NOT NULL DEFAULT -1
);

CREATE TABLE deck(
    client TEXT NOT NULL,
    hash   TEXT NOT NULL,
    ord    REAL NOT NULL,
    played INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(client, hash)
) WITHOUT ROWID;
CREATE INDEX deck_next ON deck(client, played, ord);

CREATE TABLE history(
    id        INTEGER PRIMARY KEY,
    client    TEXT NOT NULL,
    hash      TEXT NOT NULL,
    cycle     INTEGER NOT NULL,
    served_at INTEGER NOT NULL
);
CREATE INDEX history_client ON history(client, id);
"#;

/// Uniform random REAL in [0,1) from SQLite's 64-bit random().
const RAND01: &str = "((random() & 9223372036854775807) / 9223372036854775807.0)";
const ELIGIBLE: &str = "status = 'ok' AND missing = 0 AND hash IS NOT NULL";

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    pub fn open(path: &str) -> Result<Db> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        let conn = Connection::open(path).with_context(|| format!("opening database {path}"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_memory() -> Result<Db> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Db> {
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if v < 1 {
            conn.execute_batch(SCHEMA_V1)?;
            conn.pragma_update(None, "user_version", 1)?;
        }
        Ok(Db(Arc::new(Mutex::new(conn))))
    }

    /// Run a closure on the blocking pool with exclusive access to the connection.
    pub async fn run<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || db.with(f)).await?
    }

    /// Synchronous variant for code already on a blocking thread.
    pub fn with<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn parse_date(s: &str) -> Result<i64> {
    let d = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").context("expected YYYY-MM-DD")?;
    Ok(d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp())
}

pub fn generation(conn: &Connection) -> Result<i64> {
    Ok(conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key='generation'",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// Signal that the set of eligible files may have changed; client decks re-sync lazily.
pub fn bump_generation(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key,value) VALUES('generation','1')
         ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + 1",
        [],
    )?;
    Ok(())
}

fn prefix_clause(col: &str, prefixes: &[String], params: &mut Vec<Value>) -> String {
    let parts: Vec<String> = prefixes
        .iter()
        .map(|p| {
            let mut p = p.trim_start_matches('/').to_string();
            if !p.is_empty() && !p.ends_with('/') {
                p.push('/');
            }
            params.push(Value::Integer(p.len() as i64));
            params.push(Value::Text(p));
            format!("substr({col},1,?)=?")
        })
        .collect();
    format!("({})", parts.join(" OR "))
}

/// `SELECT hash FROM files WHERE <eligible + filters> GROUP BY hash`, with its parameters.
/// Content is eligible if any non-missing copy matches the filters.
pub fn eligible_sql(f: &Filters) -> Result<(String, Vec<Value>)> {
    let mut sql = format!("SELECT hash FROM files WHERE {ELIGIBLE}");
    let mut p: Vec<Value> = Vec::new();
    match f.orientation {
        Some(Orientation::Landscape) => sql.push_str(" AND width > height"),
        Some(Orientation::Portrait) => sql.push_str(" AND height > width"),
        Some(Orientation::Square) => sql.push_str(" AND width = height"),
        None => {}
    }
    if let Some(w) = f.min_width {
        sql.push_str(" AND width >= ?");
        p.push(Value::Integer(w as i64));
    }
    if let Some(h) = f.min_height {
        sql.push_str(" AND height >= ?");
        p.push(Value::Integer(h as i64));
    }
    if !f.include_paths.is_empty() {
        sql.push_str(" AND ");
        sql.push_str(&prefix_clause("path", &f.include_paths, &mut p));
    }
    if !f.exclude_paths.is_empty() {
        sql.push_str(" AND NOT ");
        sql.push_str(&prefix_clause("path", &f.exclude_paths, &mut p));
    }
    if let Some(d) = &f.taken_after {
        sql.push_str(" AND taken_at >= ?");
        p.push(Value::Integer(parse_date(d)?));
    }
    if let Some(d) = &f.taken_before {
        sql.push_str(" AND taken_at < ?");
        p.push(Value::Integer(parse_date(d)?));
    }
    sql.push_str(" GROUP BY hash");
    Ok((sql, p))
}

/// Bring a client's deck in line with the current set of eligible files:
/// drop cards that are no longer eligible, add new ones at random positions among the remaining cards.
fn sync_deck(conn: &Connection, client: &str, filters: &Filters, floor: f64) -> Result<()> {
    let (sel, fparams) = eligible_sql(filters)?;

    let mut p = vec![Value::Text(client.to_string())];
    p.extend(fparams.iter().cloned());
    conn.execute(
        &format!("DELETE FROM deck WHERE client = ? AND hash NOT IN ({sel})"),
        params_from_iter(p),
    )?;

    let mut p = vec![
        Value::Text(client.to_string()),
        Value::Real(floor),
        Value::Real(floor),
    ];
    p.extend(fparams);
    conn.execute(
        &format!(
            "INSERT OR IGNORE INTO deck(client, hash, ord, played)
             SELECT ?, hash, ? + (1.0 - ?) * {RAND01}, 0 FROM ({sel})"
        ),
        params_from_iter(p),
    )?;
    Ok(())
}

pub fn resolve_path(conn: &Connection, hash: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            &format!("SELECT path FROM files WHERE hash = ?1 AND {ELIGIBLE} ORDER BY id LIMIT 1"),
            [hash],
            |r| r.get(0),
        )
        .optional()?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectMode {
    /// Advance only if the client's interval has elapsed (or nothing has been served yet).
    Auto,
    /// Never advance (unless nothing has ever been served).
    Peek,
    /// Always advance.
    Force,
}

#[derive(Debug, Clone, Serialize)]
pub struct Selected {
    pub hash: String,
    pub rel_path: String,
    pub cycle: i64,
    pub deck_size: i64,
    pub deck_remaining: i64,
    /// When the current image was first served (unix seconds).
    pub since: i64,
    pub advanced: bool,
}

pub fn select_image(
    conn: &mut Connection,
    client: &str,
    cfg: &ClientCfg,
    mode: SelectMode,
    now: i64,
) -> Result<Option<Selected>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT OR IGNORE INTO clients(name, created_at, last_seen) VALUES(?1, ?2, ?2)",
        params![client, now],
    )?;
    tx.execute(
        "UPDATE clients SET last_seen = ?2 WHERE name = ?1",
        params![client, now],
    )?;

    let (cycle, floor, cur_hash, cur_since, synced): (i64, f64, Option<String>, Option<i64>, i64) = tx.query_row(
        "SELECT cycle, deck_floor, current_hash, current_since, synced_generation FROM clients WHERE name = ?1",
        [client],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;

    let gen = generation(&tx)?;
    if synced != gen {
        sync_deck(&tx, client, &cfg.filters, floor)?;
        tx.execute(
            "UPDATE clients SET synced_generation = ?2 WHERE name = ?1",
            params![client, gen],
        )?;
    }

    let counts = |tx: &Connection| -> Result<(i64, i64)> {
        Ok(tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(played = 0), 0) FROM deck WHERE client = ?1",
            [client],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    };

    // The currently displayed image, if it can still be resolved on disk.
    let current = match (&cur_hash, cur_since) {
        (Some(h), Some(since)) => resolve_path(&tx, h)?.map(|p| (h.clone(), p, since)),
        _ => None,
    };

    let advance = match (mode, &current) {
        (SelectMode::Force, _) | (_, None) => true,
        (SelectMode::Peek, Some(_)) => false,
        (SelectMode::Auto, Some((_, _, since))) => now - since >= cfg.interval_secs as i64,
    };

    if !advance {
        let (h, p, since) = current.unwrap();
        let (size, remaining) = counts(&tx)?;
        tx.commit()?;
        return Ok(Some(Selected {
            hash: h,
            rel_path: p,
            cycle,
            deck_size: size,
            deck_remaining: remaining,
            since,
            advanced: false,
        }));
    }

    let mut cycle = cycle;
    let mut reshuffled = false;
    let (hash, path, ord) = loop {
        let next: Option<(String, f64)> = tx
            .query_row(
                "SELECT hash, ord FROM deck WHERE client = ?1 AND played = 0 ORDER BY ord LIMIT 1",
                [client],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match next {
            Some((h, ord)) => match resolve_path(&tx, &h)? {
                Some(p) => break (h, p, ord),
                None => {
                    // Stale card (file vanished between syncs): drop it and keep going.
                    tx.execute(
                        "DELETE FROM deck WHERE client = ?1 AND hash = ?2",
                        params![client, h],
                    )?;
                }
            },
            None => {
                let (size, _) = counts(&tx)?;
                if size == 0 || reshuffled {
                    tx.commit()?;
                    return Ok(None);
                }
                // Every card has been shown: new cycle, fresh shuffle.
                tx.execute(
                    &format!("UPDATE deck SET played = 0, ord = {RAND01} WHERE client = ?1"),
                    [client],
                )?;
                if let Some(h) = &cur_hash {
                    // Don't open the new cycle with the image that is already on the screen.
                    tx.execute(
                        &format!("UPDATE deck SET ord = 0.5 + 0.5 * {RAND01} WHERE client = ?1 AND hash = ?2"),
                        params![client, h],
                    )?;
                }
                cycle += 1;
                tx.execute(
                    "UPDATE clients SET cycle = ?2, deck_floor = 0 WHERE name = ?1",
                    params![client, cycle],
                )?;
                reshuffled = true;
            }
        }
    };

    tx.execute(
        "UPDATE deck SET played = 1 WHERE client = ?1 AND hash = ?2",
        params![client, hash],
    )?;
    tx.execute(
        "UPDATE clients SET deck_floor = ?2, current_hash = ?3, current_since = ?4, served_total = served_total + 1
         WHERE name = ?1",
        params![client, ord, hash, now],
    )?;
    tx.execute(
        "INSERT INTO history(client, hash, cycle, served_at) VALUES(?1, ?2, ?3, ?4)",
        params![client, hash, cycle, now],
    )?;
    let (size, remaining) = counts(&tx)?;
    tx.commit()?;
    Ok(Some(Selected {
        hash,
        rel_path: path,
        cycle,
        deck_size: size,
        deck_remaining: remaining,
        since: now,
        advanced: true,
    }))
}

/// Mark a file as missing (it vanished from disk since the last scan).
pub fn mark_missing(conn: &Connection, rel_path: &str) -> Result<()> {
    conn.execute(
        "UPDATE files SET missing = 1, last_seen = ?2 WHERE path = ?1",
        params![rel_path, now_unix()],
    )?;
    bump_generation(conn)
}

/// Throw away a client's deck so its next request starts a fresh shuffle.
pub fn reset_client(conn: &Connection, client: &str) -> Result<bool> {
    conn.execute("DELETE FROM deck WHERE client = ?1", [client])?;
    let n = conn.execute(
        "UPDATE clients SET synced_generation = -1, deck_floor = 0, cycle = cycle + 1, current_hash = NULL,
                current_since = NULL WHERE name = ?1",
        [client],
    )?;
    Ok(n > 0)
}

// ---------------------------------------------------------------- stats

#[derive(Debug, Default, Clone, Serialize)]
pub struct LibraryStats {
    pub files_total: i64,
    pub files_ok: i64,
    pub files_pending: i64,
    pub files_unsupported: i64,
    pub files_corrupt: i64,
    pub files_missing: i64,
    pub unique_images: i64,
    pub duplicate_files: i64,
    pub landscape: i64,
    pub portrait: i64,
    pub square: i64,
    pub total_bytes: i64,
    pub by_format: Vec<(String, i64)>,
    pub generation: i64,
    pub db_bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClientStats {
    pub name: String,
    pub cycle: i64,
    pub deck_size: i64,
    pub deck_remaining: i64,
    pub served_total: i64,
    pub last_seen: i64,
    pub current_since: Option<i64>,
    pub current_id: Option<String>,
}

pub fn library_stats(conn: &Connection) -> Result<LibraryStats> {
    let mut s = LibraryStats::default();
    let (total, ok, pending, unsupported, corrupt, missing, bytes): (
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) = conn.query_row(
        "SELECT
               COALESCE(SUM(missing = 0), 0),
               COALESCE(SUM(missing = 0 AND status = 'ok'), 0),
               COALESCE(SUM(missing = 0 AND status = 'pending'), 0),
               COALESCE(SUM(missing = 0 AND status = 'unsupported'), 0),
               COALESCE(SUM(missing = 0 AND status = 'corrupt'), 0),
               COALESCE(SUM(missing = 1), 0),
               COALESCE(SUM(CASE WHEN missing = 0 THEN size END), 0)
             FROM files",
        [],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        },
    )?;
    s.files_total = total;
    s.files_ok = ok;
    s.files_pending = pending;
    s.files_unsupported = unsupported;
    s.files_corrupt = corrupt;
    s.files_missing = missing;
    s.total_bytes = bytes;
    s.unique_images = conn.query_row(
        &format!("SELECT COUNT(DISTINCT hash) FROM files WHERE {ELIGIBLE}"),
        [],
        |r| r.get(0),
    )?;
    s.duplicate_files = ok - s.unique_images;
    let (l, p, q): (i64, i64, i64) = conn.query_row(
        &format!(
            "SELECT COALESCE(SUM(width > height),0), COALESCE(SUM(height > width),0), COALESCE(SUM(width = height),0)
             FROM files WHERE {ELIGIBLE}"
        ),
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    s.landscape = l;
    s.portrait = p;
    s.square = q;
    let mut st = conn.prepare(&format!(
        "SELECT COALESCE(format,'unknown'), COUNT(*) FROM files WHERE {ELIGIBLE} GROUP BY 1 ORDER BY 2 DESC"
    ))?;
    s.by_format = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    s.generation = generation(conn)?;
    s.db_bytes = conn.query_row(
        "SELECT (SELECT page_count FROM pragma_page_count) * (SELECT page_size FROM pragma_page_size)",
        [],
        |r| r.get(0),
    )?;
    Ok(s)
}

pub fn client_stats(conn: &Connection) -> Result<Vec<ClientStats>> {
    let mut st = conn.prepare(
        "SELECT c.name, c.cycle, c.served_total, c.last_seen, c.current_since, c.current_hash,
                (SELECT COUNT(*) FROM deck d WHERE d.client = c.name),
                (SELECT COUNT(*) FROM deck d WHERE d.client = c.name AND d.played = 0)
         FROM clients c ORDER BY c.name",
    )?;
    let rows = st
        .query_map([], |r| {
            let hash: Option<String> = r.get(5)?;
            Ok(ClientStats {
                name: r.get(0)?,
                cycle: r.get(1)?,
                served_total: r.get(2)?,
                last_seen: r.get(3)?,
                current_since: r.get(4)?,
                current_id: hash.map(|h| h.chars().take(16).collect()),
                deck_size: r.get(6)?,
                deck_remaining: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn has_eligible(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM files WHERE {ELIGIBLE})"),
        [],
        |r| r.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn add(conn: &Connection, path: &str, hash: &str, w: i64, h: i64) {
        conn.execute(
            "INSERT INTO files(path,size,mtime_ns,hash,width,height,format,status,first_seen,last_seen)
             VALUES(?1,1,1,?2,?3,?4,'jpeg','ok',0,0)",
            params![path, hash, w, h],
        )
        .unwrap();
        bump_generation(conn).unwrap();
    }

    fn cfg(interval: u64) -> ClientCfg {
        ClientCfg {
            interval_secs: interval,
            ..Default::default()
        }
    }

    fn next(conn: &mut Connection, c: &ClientCfg, now: i64) -> Selected {
        select_image(conn, "k", c, SelectMode::Force, now)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn no_repeats_until_every_image_shown() {
        let db = Db::open_memory().unwrap();
        db.with(|conn| {
            for i in 0..50 {
                add(conn, &format!("a/{i}.jpg"), &format!("h{i}"), 4000, 3000);
            }
            let c = cfg(0);
            let mut seen = HashSet::new();
            let mut prev = String::new();
            for i in 0..50 {
                let s = next(conn, &c, 100 + i);
                prev = s.hash.clone();
                assert!(seen.insert(s.hash), "repeat inside a cycle");
                assert_eq!(s.cycle, 1);
                assert_eq!(s.deck_remaining, 49 - i);
            }
            // Next pull starts cycle 2 and must not repeat the image currently on screen.
            let last = next(conn, &c, 200);
            assert_eq!(last.cycle, 2);
            assert_ne!(
                last.hash, prev,
                "new cycle must not open with the image already on screen"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn exact_duplicates_are_shown_once() {
        let db = Db::open_memory().unwrap();
        db.with(|conn| {
            add(conn, "a.jpg", "same", 100, 100);
            add(conn, "copy/a.jpg", "same", 100, 100);
            add(conn, "b.jpg", "other", 100, 100);
            let c = cfg(0);
            let a = next(conn, &c, 1);
            let b = next(conn, &c, 2);
            assert_ne!(a.hash, b.hash);
            assert_eq!(a.deck_size, 2);
            assert_eq!(next(conn, &c, 3).cycle, 2);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn new_files_join_the_remaining_deck_and_removed_files_leave() {
        let db = Db::open_memory().unwrap();
        db.with(|conn| {
            for i in 0..10 {
                add(conn, &format!("{i}.jpg"), &format!("h{i}"), 10, 10);
            }
            let c = cfg(0);
            let mut shown = HashSet::new();
            for i in 0..4 {
                shown.insert(next(conn, &c, i).hash);
            }
            // A file shows up mid-cycle and one that was already shown is deleted.
            add(conn, "new.jpg", "hnew", 10, 10);
            let gone = shown.iter().next().unwrap().clone();
            conn.execute("UPDATE files SET missing = 1 WHERE hash = ?1", [&gone])
                .unwrap();
            bump_generation(conn).unwrap();

            let mut rest = HashSet::new();
            loop {
                let s = next(conn, &c, 10 + rest.len() as i64);
                if s.cycle != 1 {
                    break;
                }
                assert!(!shown.contains(&s.hash), "repeat within the cycle");
                rest.insert(s.hash);
            }
            assert!(
                rest.contains("hnew"),
                "new file was not offered in the current cycle"
            );
            assert_eq!(rest.len(), 7, "6 unseen + 1 new");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn sticky_interval_only_advances_when_due() {
        let db = Db::open_memory().unwrap();
        db.with(|conn| {
            for i in 0..5 {
                add(conn, &format!("{i}.jpg"), &format!("h{i}"), 10, 10);
            }
            let c = cfg(3600);
            let first = select_image(conn, "k", &c, SelectMode::Auto, 1000)
                .unwrap()
                .unwrap();
            assert!(first.advanced);
            let again = select_image(conn, "k", &c, SelectMode::Auto, 1000 + 3599)
                .unwrap()
                .unwrap();
            assert!(!again.advanced);
            assert_eq!(again.hash, first.hash);
            let peek = select_image(conn, "k", &c, SelectMode::Peek, 1000 + 9999)
                .unwrap()
                .unwrap();
            assert_eq!(peek.hash, first.hash);
            let due = select_image(conn, "k", &c, SelectMode::Auto, 1000 + 3600)
                .unwrap()
                .unwrap();
            assert!(due.advanced);
            assert_ne!(due.hash, first.hash);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn clients_have_independent_decks_and_filters() {
        let db = Db::open_memory().unwrap();
        db.with(|conn| {
            add(conn, "land1.jpg", "l1", 400, 300);
            add(conn, "land2.jpg", "l2", 400, 300);
            add(conn, "port1.jpg", "p1", 300, 400);
            let mut landscape = cfg(0);
            landscape.filters.orientation = Some(Orientation::Landscape);
            let any = cfg(0);
            for i in 0..4 {
                let s = select_image(conn, "wide", &landscape, SelectMode::Force, i)
                    .unwrap()
                    .unwrap();
                assert!(s.hash.starts_with('l'));
                assert_eq!(s.deck_size, 2);
            }
            let s = select_image(conn, "all", &any, SelectMode::Force, 9)
                .unwrap()
                .unwrap();
            assert_eq!(s.deck_size, 3);
            assert_eq!(s.cycle, 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn path_and_size_filters() {
        let db = Db::open_memory().unwrap();
        db.with(|conn| {
            add(conn, "Family/2024/a.jpg", "a", 4000, 3000);
            add(conn, "Family/2024/small.jpg", "s", 300, 200);
            add(conn, "Family2/b.jpg", "b", 4000, 3000);
            add(conn, "Screenshots/c.jpg", "c", 4000, 3000);
            let mut c = cfg(0);
            c.filters.include_paths = vec!["Family".into()];
            c.filters.min_width = Some(1000);
            let s = select_image(conn, "k", &c, SelectMode::Force, 1)
                .unwrap()
                .unwrap();
            assert_eq!((s.hash.as_str(), s.deck_size), ("a", 1)); // "Family2" must not match "Family"
            let mut c2 = cfg(0);
            c2.filters.exclude_paths = vec!["Screenshots/".into(), "Family2".into()];
            let s = select_image(conn, "k2", &c2, SelectMode::Force, 1)
                .unwrap()
                .unwrap();
            assert_eq!(s.deck_size, 2);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn empty_library_yields_none() {
        let db = Db::open_memory().unwrap();
        db.with(|conn| {
            assert!(select_image(conn, "k", &cfg(0), SelectMode::Auto, 1)
                .unwrap()
                .is_none());
            Ok(())
        })
        .unwrap();
    }
}
