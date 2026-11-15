//! Configuration (TOML) with sensible defaults. Everything is optional.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerCfg,
    #[serde(default)]
    pub library: LibraryCfg,
    #[serde(default)]
    pub storage: StorageCfg,
    #[serde(default)]
    pub scan: ScanCfg,
    #[serde(default)]
    pub processing: ProcessingCfg,
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
    #[serde(default)]
    pub default_client: ClientCfg,
    #[serde(default)]
    pub clients: HashMap<String, ClientCfg>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerCfg {
    #[serde(default = "d_bind")]
    pub bind: String,
    /// If set, image endpoints require `?token=...` or `Authorization: Bearer ...`.
    pub token: Option<String>,
    /// If set, /admin/* endpoints require this token (same two ways).
    pub admin_token: Option<String>,
    /// Default IANA timezone for time windows (clients may override).
    #[serde(default = "d_tz")]
    pub timezone: String,
    /// Max simultaneous image conversions.
    #[serde(default = "d_render_conc")]
    pub render_concurrency: usize,
}
impl Default for ServerCfg {
    fn default() -> Self {
        toml::from_str("").unwrap()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryCfg {
    #[serde(default = "d_dir")]
    pub dir: String,
    #[serde(default = "d_exts")]
    pub extensions: Vec<String>,
    /// Directory names that are never descended into (plus anything starting with '.').
    #[serde(default = "d_skip")]
    pub skip_dirs: Vec<String>,
}
impl Default for LibraryCfg {
    fn default() -> Self {
        toml::from_str("").unwrap()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageCfg {
    #[serde(default = "d_db")]
    pub db_path: String,
    #[serde(default = "d_cache")]
    pub cache_dir: String,
    #[serde(default = "d_cache_mb")]
    pub cache_max_mb: u64,
}
impl Default for StorageCfg {
    fn default() -> Self {
        toml::from_str("").unwrap()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanCfg {
    #[serde(default = "yes")]
    pub on_startup: bool,
    /// Hashing/metadata worker threads.
    #[serde(default = "d_threads")]
    pub threads: usize,
    /// Enable inotify-based change detection (local filesystems only).
    #[serde(default)]
    pub watch: bool,
    #[serde(default = "d_debounce")]
    pub watch_debounce_secs: u64,
    /// Optional periodic incremental rescan (0 = off). Useful on NFS/SMB where inotify can't work.
    #[serde(default)]
    pub periodic_minutes: u64,
}
impl Default for ScanCfg {
    fn default() -> Self {
        toml::from_str("").unwrap()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessingCfg {
    /// false (default): serve the original files byte-for-byte. The server only chooses *which*
    /// image to show; the display (e.g. FrameOS) does its own sizing, colour handling and dithering.
    /// true: convert every image to the client's `profile` (resize, blurred padding, dithering, ...).
    #[serde(default)]
    pub enabled: bool,
    /// Only used when `enabled = false`. HEIC/HEIF files are transcoded to a full-size JPEG (no resize,
    /// no dithering) because most displays/firmware cannot decode HEIC. Set to false to serve the
    /// raw .heic bytes, or drop "heic"/"heif" from `library.extensions` to leave them out entirely.
    #[serde(default = "yes")]
    pub convert_heic: bool,
    #[serde(default = "d_heic_quality")]
    pub heic_quality: u8,
}
impl Default for ProcessingCfg {
    fn default() -> Self {
        toml::from_str("").unwrap()
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    /// Crop to fill if the loss is small (`max_crop`), otherwise fit inside and pad with a blurred copy.
    #[default]
    BlurPad,
    /// Always crop to fill.
    Cover,
    /// Always fit inside and pad with a solid `background`.
    Contain,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    #[default]
    Jpeg,
    Png,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Dither {
    #[default]
    None,
    FloydSteinberg,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub fit: Fit,
    /// BlurPad: crop instead of padding when the cropped fraction would be <= this.
    #[serde(default = "d_max_crop")]
    pub max_crop: f32,
    #[serde(default = "d_sigma")]
    pub blur_sigma: f32,
    /// Darken the blurred backdrop by this fraction (0..1).
    #[serde(default)]
    pub bg_dim: f32,
    #[serde(default = "d_white")]
    pub background: [u8; 3],
    #[serde(default)]
    pub format: Format,
    #[serde(default = "d_quality")]
    pub quality: u8,
    #[serde(default = "d_one")]
    pub contrast: f32,
    #[serde(default = "d_one")]
    pub saturation: f32,
    #[serde(default)]
    pub dither: Dither,
    /// Colours the panel really shows (used for matching when dithering).
    pub palette: Option<Vec<[u8; 3]>>,
    /// Colours written to the output file (defaults to `palette`). Many firmwares expect pure colours.
    pub output_palette: Option<Vec<[u8; 3]>>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    Landscape,
    Portrait,
    Square,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filters {
    pub orientation: Option<Orientation>,
    pub min_width: Option<u32>,
    pub min_height: Option<u32>,
    /// Directory prefixes (relative to the library root). Empty = everything.
    #[serde(default)]
    pub include_paths: Vec<String>,
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    /// EXIF capture date range, "YYYY-MM-DD". Files without an EXIF date never match when set.
    pub taken_after: Option<String>,
    pub taken_before: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutsideWindow {
    /// Keep serving the current image (never advances) with X-Next-Refresh set to the next opening.
    #[default]
    Current,
    /// Respond 204 No Content.
    NoContent,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowCfg {
    /// "mon".."sun"; empty = every day.
    #[serde(default)]
    pub days: Vec<String>,
    /// "HH:MM" (24h, local to the client's timezone).
    pub start: String,
    /// "HH:MM" or "24:00". end < start means the window runs past midnight.
    pub end: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientCfg {
    #[serde(default = "d_profile")]
    pub profile: String,
    /// The same image is served until this many seconds have passed; then the next one.
    /// 0 = advance on every request.
    #[serde(default = "d_interval")]
    pub interval_secs: u64,
    pub timezone: Option<String>,
    #[serde(default)]
    pub windows: Vec<WindowCfg>,
    #[serde(default)]
    pub outside_window: OutsideWindow,
    #[serde(default)]
    pub filters: Filters,
}
impl Default for ClientCfg {
    fn default() -> Self {
        toml::from_str("").unwrap()
    }
}

fn yes() -> bool {
    true
}
fn d_bind() -> String {
    "0.0.0.0:8080".into()
}
fn d_tz() -> String {
    "UTC".into()
}
fn d_render_conc() -> usize {
    2
}
fn d_dir() -> String {
    "/images".into()
}
fn d_exts() -> Vec<String> {
    ["jpg", "jpeg", "png", "webp", "bmp", "heic", "heif"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}
fn d_skip() -> Vec<String> {
    ["@eaDir", "#recycle", "$RECYCLE.BIN", "lost+found"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}
fn d_db() -> String {
    "/data/frameshuffle.db".into()
}
fn d_cache() -> String {
    "/cache".into()
}
fn d_cache_mb() -> u64 {
    2048
}
fn d_threads() -> usize {
    2
}
fn d_debounce() -> u64 {
    10
}
fn d_max_crop() -> f32 {
    0.05
}
fn d_sigma() -> f32 {
    6.0
}
fn d_white() -> [u8; 3] {
    [255, 255, 255]
}
fn d_quality() -> u8 {
    90
}
fn d_heic_quality() -> u8 {
    92
}
fn d_one() -> f32 {
    1.0
}
fn d_profile() -> String {
    "e1004".into()
}
fn d_interval() -> u64 {
    3600
}

/// Approximate E Ink Spectra 6 colours as the panel actually renders them (muted vs. pure RGB).
/// These are starting points; override `palette` in a profile once you've calibrated your panel.
pub const SPECTRA6_APPROX: [[u8; 3]; 6] = [
    [25, 30, 33],    // black
    [232, 232, 232], // white
    [178, 19, 24],   // red
    [18, 95, 32],    // green
    [33, 64, 154],   // blue
    [239, 222, 68],  // yellow
];
/// Pure colours, which is what most Spectra 6 firmware/driver code expects in a pre-dithered image.
pub const SPECTRA6_PURE: [[u8; 3]; 6] = [
    [0, 0, 0],
    [255, 255, 255],
    [255, 0, 0],
    [0, 255, 0],
    [0, 0, 255],
    [255, 255, 0],
];

fn builtin_profiles() -> HashMap<String, Profile> {
    let base = |w: u32, h: u32| Profile {
        width: w,
        height: h,
        fit: Fit::BlurPad,
        max_crop: d_max_crop(),
        blur_sigma: d_sigma(),
        bg_dim: 0.0,
        background: d_white(),
        format: Format::Jpeg,
        quality: d_quality(),
        contrast: 1.0,
        saturation: 1.0,
        dither: Dither::None,
        palette: None,
        output_palette: None,
    };
    let mut m = HashMap::new();
    // reTerminal E1004 mounted landscape (native panel is 1200x1600 portrait; FrameOS rotates).
    // Undithered: FrameOS dithers on the device.
    m.insert("e1004".to_string(), base(1600, 1200));
    // Same, but pre-dithered to Spectra 6 as an indexed PNG, for firmware that just blits pixels.
    m.insert(
        "e1004-dithered".to_string(),
        Profile {
            format: Format::Png,
            dither: Dither::FloydSteinberg,
            palette: Some(SPECTRA6_APPROX.to_vec()),
            output_palette: Some(SPECTRA6_PURE.to_vec()),
            ..base(1600, 1200)
        },
    );
    // Sample for a future Android frame.
    m.insert(
        "android-1080p".to_string(),
        Profile {
            quality: 85,
            ..base(1920, 1080)
        },
    );
    m
}

impl Config {
    /// Parse TOML text (empty = all defaults) and add the built-in profiles.
    pub fn parse(text: &str) -> Result<Config> {
        let mut cfg: Config = toml::from_str(text).context("parsing config")?;
        // Built-in profiles are always available; user-defined ones override by name.
        for (k, v) in builtin_profiles() {
            cfg.profiles.entry(k).or_insert(v);
        }
        Ok(cfg)
    }

    /// Load the config file (optional), apply environment overrides, validate.
    pub fn load(path: Option<&str>) -> Result<Config> {
        let text = match path {
            Some(p) if std::path::Path::new(p).exists() => {
                std::fs::read_to_string(p).with_context(|| format!("reading {p}"))?
            }
            Some(p) => {
                eprintln!("config file {p} not found, using defaults");
                String::new()
            }
            None => String::new(),
        };
        let mut cfg = Config::parse(&text)?;
        let applied = apply_env(&mut cfg, &|k| std::env::var(k).ok())?;
        if !applied.is_empty() {
            tracing::info!(
                "settings overridden from the environment: {}",
                applied.join(", ")
            );
        }
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if !(1..=100).contains(&self.processing.heic_quality) {
            bail!("processing.heic_quality must be 1..=100");
        }
        for (name, p) in &self.profiles {
            if p.width == 0 || p.height == 0 || p.width > 8192 || p.height > 8192 {
                bail!("profile {name}: width/height must be 1..=8192");
            }
            if !(1..=100).contains(&p.quality) {
                bail!("profile {name}: quality must be 1..=100");
            }
            if p.dither != Dither::None && p.palette.as_ref().is_none_or(|v| v.len() < 2) {
                bail!("profile {name}: dithering needs a `palette` with at least 2 colours");
            }
            if let (Some(a), Some(b)) = (&p.palette, &p.output_palette) {
                if a.len() != b.len() {
                    bail!("profile {name}: palette and output_palette must have the same length");
                }
            }
            if let Some(a) = &p.palette {
                if a.len() > 16 {
                    bail!("profile {name}: palette may have at most 16 colours");
                }
            }
        }
        let mut all: Vec<(&str, &ClientCfg)> = vec![("default_client", &self.default_client)];
        all.extend(self.clients.iter().map(|(k, v)| (k.as_str(), v)));
        for (name, c) in all {
            if !self.profiles.contains_key(&c.profile) {
                bail!("client {name}: unknown profile '{}'", c.profile);
            }
            crate::schedule::Windows::new(
                c.timezone.as_deref().unwrap_or(&self.server.timezone),
                &c.windows,
            )
            .with_context(|| format!("client {name}: invalid timezone/windows"))?;
            for d in [&c.filters.taken_after, &c.filters.taken_before]
                .into_iter()
                .flatten()
            {
                crate::db::parse_date(d)
                    .with_context(|| format!("client {name}: bad date '{d}'"))?;
            }
        }
        Ok(())
    }

    pub fn client(&self, name: &str) -> &ClientCfg {
        self.clients.get(name).unwrap_or(&self.default_client)
    }
}

pub fn valid_client_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn parse_bool(name: &str, v: &str) -> Result<bool> {
    match v.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("{name}: expected true/false, got '{v}'"),
    }
}

fn parse_num<T: std::str::FromStr>(name: &str, v: &str) -> Result<T> {
    v.parse()
        .map_err(|_| anyhow::anyhow!("{name}: invalid number '{v}'"))
}

/// Environment variables override the config file (handy for Docker `.env` files). Empty values are
/// treated as unset. `get` is injectable so this can be tested without touching the real environment.
/// Returns the names of the variables that were applied (never their values: some are secrets).
pub fn apply_env(
    cfg: &mut Config,
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<&'static str>> {
    let mut applied: Vec<&'static str> = Vec::new();
    let val = |k: &str| {
        get(k)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };

    macro_rules! text {
        ($name:literal => $target:expr) => {
            if let Some(v) = val($name) {
                $target = v;
                applied.push($name);
            }
        };
    }
    macro_rules! opt_text {
        ($name:literal => $target:expr) => {
            if let Some(v) = val($name) {
                $target = Some(v);
                applied.push($name);
            }
        };
    }
    macro_rules! num {
        ($name:literal => $target:expr) => {
            if let Some(v) = val($name) {
                $target = parse_num($name, &v)?;
                applied.push($name);
            }
        };
    }
    macro_rules! flag {
        ($name:literal => $target:expr) => {
            if let Some(v) = val($name) {
                $target = parse_bool($name, &v)?;
                applied.push($name);
            }
        };
    }

    text!("IMAGE_DIR" => cfg.library.dir); // kept for compatibility with the original Flask app
    text!("FRAMESHUFFLE_LIBRARY_DIR" => cfg.library.dir);
    text!("FRAMESHUFFLE_BIND" => cfg.server.bind);
    opt_text!("FRAMESHUFFLE_TOKEN" => cfg.server.token);
    opt_text!("FRAMESHUFFLE_ADMIN_TOKEN" => cfg.server.admin_token);
    text!("FRAMESHUFFLE_TIMEZONE" => cfg.server.timezone);
    num!("FRAMESHUFFLE_RENDER_CONCURRENCY" => cfg.server.render_concurrency);
    text!("FRAMESHUFFLE_DB_PATH" => cfg.storage.db_path);
    text!("FRAMESHUFFLE_CACHE_DIR" => cfg.storage.cache_dir);
    num!("FRAMESHUFFLE_CACHE_MAX_MB" => cfg.storage.cache_max_mb);
    flag!("FRAMESHUFFLE_SCAN_ON_STARTUP" => cfg.scan.on_startup);
    num!("FRAMESHUFFLE_SCAN_THREADS" => cfg.scan.threads);
    flag!("FRAMESHUFFLE_SCAN_WATCH" => cfg.scan.watch);
    num!("FRAMESHUFFLE_SCAN_WATCH_DEBOUNCE_SECS" => cfg.scan.watch_debounce_secs);
    num!("FRAMESHUFFLE_SCAN_PERIODIC_MINUTES" => cfg.scan.periodic_minutes);
    flag!("FRAMESHUFFLE_PROCESSING" => cfg.processing.enabled);
    flag!("FRAMESHUFFLE_CONVERT_HEIC" => cfg.processing.convert_heic);
    num!("FRAMESHUFFLE_HEIC_QUALITY" => cfg.processing.heic_quality);
    // These two shape the client used for any name not listed under [clients.*].
    text!("FRAMESHUFFLE_DEFAULT_PROFILE" => cfg.default_client.profile);
    num!("FRAMESHUFFLE_DEFAULT_INTERVAL_SECS" => cfg.default_client.interval_secs);

    // Plain TZ (set by Docker/compose) is used only when nothing more specific was given.
    if cfg.server.timezone == "UTC" && val("FRAMESHUFFLE_TIMEZONE").is_none() {
        if let Some(tz) = val("TZ") {
            if tz.parse::<chrono_tz::Tz>().is_ok() {
                cfg.server.timezone = tz;
                applied.push("TZ");
            }
        }
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn defaults_are_valid_and_processing_is_off() {
        let cfg = Config::parse("").unwrap();
        cfg.validate().unwrap();
        assert!(!cfg.processing.enabled);
        assert!(cfg.processing.convert_heic);
        assert_eq!(cfg.default_client.interval_secs, 3600);
        assert!(cfg.profiles.contains_key("e1004"));
    }

    #[test]
    fn env_overrides_file_and_empty_means_unset() {
        let mut cfg =
            Config::parse("[server]\ntoken = \"from-file\"\n[processing]\nenabled = false\n")
                .unwrap();
        let applied = apply_env(
            &mut cfg,
            &env(&[
                ("FRAMESHUFFLE_TOKEN", ""), // empty: ignored, file value stays
                ("FRAMESHUFFLE_ADMIN_TOKEN", " s3cret "),
                ("FRAMESHUFFLE_PROCESSING", "TRUE"),
                ("FRAMESHUFFLE_SCAN_PERIODIC_MINUTES", "15"),
                ("FRAMESHUFFLE_DEFAULT_INTERVAL_SECS", "600"),
                ("TZ", "America/Los_Angeles"),
            ]),
        )
        .unwrap();
        assert_eq!(cfg.server.token.as_deref(), Some("from-file"));
        assert_eq!(cfg.server.admin_token.as_deref(), Some("s3cret"));
        assert!(cfg.processing.enabled);
        assert_eq!(cfg.scan.periodic_minutes, 15);
        assert_eq!(cfg.default_client.interval_secs, 600);
        assert_eq!(cfg.server.timezone, "America/Los_Angeles");
        assert!(
            applied.contains(&"FRAMESHUFFLE_PROCESSING")
                && !applied.contains(&"FRAMESHUFFLE_TOKEN")
        );
        cfg.validate().unwrap();
    }

    #[test]
    fn explicit_timezone_beats_tz_and_bad_values_are_errors() {
        let mut cfg = Config::parse("").unwrap();
        apply_env(
            &mut cfg,
            &env(&[
                ("FRAMESHUFFLE_TIMEZONE", "Europe/Berlin"),
                ("TZ", "Asia/Tokyo"),
            ]),
        )
        .unwrap();
        assert_eq!(cfg.server.timezone, "Europe/Berlin");

        let mut cfg = Config::parse("").unwrap();
        assert!(apply_env(&mut cfg, &env(&[("FRAMESHUFFLE_SCAN_WATCH", "maybe")])).is_err());
        assert!(apply_env(&mut cfg, &env(&[("FRAMESHUFFLE_SCAN_THREADS", "lots")])).is_err());
    }

    #[test]
    fn unknown_config_keys_are_rejected() {
        assert!(Config::parse("[server]\nbindd = \"x\"\n").is_err());
    }
}
