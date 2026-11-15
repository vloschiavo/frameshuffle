# Configuration reference

frameshuffle works with **no configuration at all**: every setting has a default. You can configure it with:

1. **Environment variables** (`FRAMESHUFFLE_*`), usually set through a `.env` file; see `.env.sample`. Good for
   deployment-level settings.
2. **A TOML file** (default `/config/config.toml`, or `--config FILE`, or `$FRAMESHUFFLE_CONFIG`). Needed for
   per-client settings, time windows, filters and custom display profiles. Start from `config/config.example.toml`.

Precedence: **environment variable > config file > built-in default.** An empty environment variable counts as
unset. Unknown keys in the TOML file are rejected at startup, so typos fail loudly instead of being ignored.

## Environment variables

| Variable | Config key | Default | Notes |
|---|---|---|---|
| `FRAMESHUFFLE_LIBRARY_DIR` (or `IMAGE_DIR`) | `library.dir` | `/images` | |
| `FRAMESHUFFLE_BIND` | `server.bind` | `0.0.0.0:8080` | |
| `FRAMESHUFFLE_TOKEN` | `server.token` | none | Protects image endpoints |
| `FRAMESHUFFLE_ADMIN_TOKEN` | `server.admin_token` | none | Protects `/admin/*`, `/stats`, `/metrics` |
| `FRAMESHUFFLE_TIMEZONE` | `server.timezone` | `UTC` | IANA name. Plain `TZ` is used if this isn't set |
| `FRAMESHUFFLE_RENDER_CONCURRENCY` | `server.render_concurrency` | `2` | |
| `FRAMESHUFFLE_DB_PATH` | `storage.db_path` | `/data/frameshuffle.db` | |
| `FRAMESHUFFLE_CACHE_DIR` | `storage.cache_dir` | `/cache` | |
| `FRAMESHUFFLE_CACHE_MAX_MB` | `storage.cache_max_mb` | `2048` | |
| `FRAMESHUFFLE_SCAN_ON_STARTUP` | `scan.on_startup` | `true` | |
| `FRAMESHUFFLE_SCAN_THREADS` | `scan.threads` | `2` | |
| `FRAMESHUFFLE_SCAN_WATCH` | `scan.watch` | `false` | |
| `FRAMESHUFFLE_SCAN_WATCH_DEBOUNCE_SECS` | `scan.watch_debounce_secs` | `10` | |
| `FRAMESHUFFLE_SCAN_PERIODIC_MINUTES` | `scan.periodic_minutes` | `0` | |
| `FRAMESHUFFLE_PROCESSING` | `processing.enabled` | `false` | |
| `FRAMESHUFFLE_CONVERT_HEIC` | `processing.convert_heic` | `true` | Only has an effect in a build with HEIC support (`WITH_HEIC=1`) |
| `FRAMESHUFFLE_HEIC_QUALITY` | `processing.heic_quality` | `92` | |
| `FRAMESHUFFLE_DEFAULT_PROFILE` | `default_client.profile` | `e1004` | |
| `FRAMESHUFFLE_DEFAULT_INTERVAL_SECS` | `default_client.interval_secs` | `3600` | |
| `RUST_LOG` | n/a | `info` | `debug` adds a line per request |

Booleans accept `true/false`, `1/0`, `yes/no`, `on/off`. The two `default_client` variables only affect clients that
are *not* listed under `[clients.*]`.

## TOML file

```toml
[server]
bind = "0.0.0.0:8080"
token = "..."                # default: none. If set, image endpoints need ?token=... or "Authorization: Bearer ..."
admin_token = "..."          # default: none. Protects /admin/*, /stats and /metrics. Also accepted on image endpoints
timezone = "UTC"             # IANA name, e.g. "America/Los_Angeles". Used for time windows
render_concurrency = 2       # simultaneous conversions (HEIC transcoding included)

[library]
dir = "/images"
extensions = ["jpg", "jpeg", "png", "webp", "bmp", "heic", "heif"]
skip_dirs = ["@eaDir", "#recycle", "$RECYCLE.BIN", "lost+found"]   # anything starting with "." is always skipped

[storage]
db_path = "/data/frameshuffle.db"
cache_dir = "/cache"
cache_max_mb = 2048          # oldest cached files are evicted past this

[scan]
on_startup = true            # incremental, so restarts are cheap
threads = 2                  # hashing workers, clamped to 1..32
watch = false                # true = inotify change detection (local filesystems only)
watch_debounce_secs = 10
periodic_minutes = 0         # >0 = rescan on a timer (use this instead of watch on NFS/SMB)

[processing]
enabled = false              # false = serve originals untouched; true = convert to the client's profile
convert_heic = true          # only in a WITH_HEIC=1 build, when enabled = false: HEIC -> full-size JPEG (no resize/dither)
heic_quality = 92            # 1..100

# Profiles are only used when processing.enabled = true.
# Built in: "e1004" (1600x1200), "e1004-dithered", "android-1080p" (1920x1080, quality 85).
# Defining a profile with the same name replaces the built-in.
[profiles.NAME]
width = 1600                 # required, 1..8192
height = 1200                # required, 1..8192
fit = "blur_pad"             # blur_pad | cover | contain
max_crop = 0.05              # blur_pad: crop instead of padding if <= this fraction of the picture is lost
blur_sigma = 6.0             # blur_pad backdrop blur strength
bg_dim = 0.0                 # 0..1, darkens the blurred backdrop
background = [255, 255, 255] # contain: padding colour
format = "jpeg"              # jpeg | png
quality = 90                 # 1..100, JPEG only
contrast = 1.0
saturation = 1.0
dither = "none"              # none | floyd_steinberg
palette = [[r,g,b], ...]     # required if dithering: 2..16 colours, as the panel really renders them
output_palette = [[r,g,b], ...]  # colours written to the file; same length as palette; defaults to palette

# [default_client] is used for any client name not listed. [clients.NAME] takes the same keys.
[default_client]
profile = "e1004"            # must exist in the profiles
interval_secs = 3600         # same image until this elapses; 0 = advance on every request
timezone = "..."             # default: server.timezone
outside_window = "current"   # current | no_content (204)

[[default_client.windows]]
days = ["mon", "tue"]        # mon..sun or full names; empty/omitted = every day
start = "07:00"              # HH:MM, 24h
end = "22:00"                # HH:MM or "24:00"; end earlier than start crosses midnight

[default_client.filters]
orientation = "landscape"    # landscape | portrait | square
min_width = 1200
min_height = 800
include_paths = ["Family", "Travel/2024"]   # directory prefixes relative to library.dir
exclude_paths = ["Screenshots"]
taken_after = "2015-01-01"   # EXIF capture date, YYYY-MM-DD; files without an EXIF date never match
taken_before = "2027-01-01"

[clients.kitchen]            # name: letters, digits, - _ . (max 64)
# ...same keys as [default_client]
```

## Things that may surprise you

0. **The official image can't read HEIC.** `heic`/`heif` stay in `library.extensions` by default so that such files are
   reported (as `unsupported`, with a log warning) instead of silently ignored. Convert them with
   `scripts/convert-heic.sh` or build with `WITH_HEIC=1`; see the README.
1. **Clients don't inherit from `[default_client]`.** A `[clients.kitchen]` section takes its own defaults for anything
   it omits (profile `e1004`, interval 3600, no filters or windows), not the values from `[default_client]`. Repeat any
   setting you want.
2. **Changing a client's filters doesn't apply on restart.** Decks only re-sync when the library changes. After editing
   filters, run `POST /admin/reset?client=NAME`.
3. **Setting only `token` leaves `/stats` and `/metrics` open.** They are protected by `admin_token`; set both to lock
   down everything.
4. **Profile edits invalidate the cache.** Each profile's cache directory is named after a fingerprint of its settings,
   so edited profiles re-render and old directories are cleaned up automatically.
