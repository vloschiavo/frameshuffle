# frameshuffle

[![CI](https://github.com/YOUR_GITHUB_USER/frameshuffle/actions/workflows/ci.yml/badge.svg)](https://github.com/YOUR_GITHUB_USER/frameshuffle/actions/workflows/ci.yml)
[![Docker Hub](https://img.shields.io/docker/v/YOUR_DOCKERHUB_USER/frameshuffle?label=docker%20hub&sort=semver)](https://hub.docker.com/r/YOUR_DOCKERHUB_USER/frameshuffle)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A local-only image server for e-paper displays and other photo frames. Point it at a folder of photos
and each frame gets one image at a time from its own shuffled deck: **no repeats until every image has
been shown**, exact duplicates shown once, new files folded into the current deck, no cloud involved.

- **Fair shuffle, per frame.** Every client (`?client=kitchen`) has its own persistent deck. Files added
  mid-cycle join the cards still to come; deleted files drop out. Decks survive restarts and renames.
- **Built for slow, battery-powered displays.** The same image is served until the frame's interval
  elapses, so retries and previews never burn an image. `X-Next-Refresh` tells the frame when to come
  back, per-client time windows keep it quiet overnight, and ETag/`304` lets it skip a refresh.
- **Handles real libraries.** Tested on 35,000 files (2 s to index and hash small files, about 50 MB RAM).
  Incremental rescans, inotify or periodic change detection, exact-duplicate detection. HEIC photos work via
  a one-time converter script or an opt-in build (see [HEIC photos](#heic-photos)).
- **Filters.** Per client: orientation, minimum size, include/exclude folders, EXIF date range.
- **Optional image processing.** Off by default (serve originals untouched). When on: resize,
  blurred-background padding, dithering to a palette (e.g. Spectra 6), JPEG/PNG output, disk cache.
- **Observable.** `/health`, `/ready`, a JSON `/stats` and Prometheus `/metrics`.
- Single Rust binary; multi-arch (amd64, arm64) Docker image.

> **HEIC note:** the published Docker image has no HEIC decoder, so `.heic` files are skipped (with a log
> warning). Convert them once with [`scripts/convert-heic.sh`](scripts/convert-heic.sh) or build the image yourself
> with `WITH_HEIC=1`. [Details and why.](#heic-photos)

## Quick start

You need Docker with Compose, and a folder of photos.

```bash
git clone https://github.com/YOUR_GITHUB_USER/frameshuffle.git && cd frameshuffle
cp .env.sample .env
$EDITOR .env                     # set PHOTOS_DIR (the only required setting)

# Use the published image ...
echo 'FRAMESHUFFLE_IMAGE=YOUR_DOCKERHUB_USER/frameshuffle:latest' >> .env
docker compose pull
# ... or skip those two lines and compose will build from source.

docker compose up -d
curl localhost:8080/ready        # 200 once the first scan has found images
curl -o photo.jpg "localhost:8080/image?client=kitchen"
```

Point your frame at `http://<host>:8080/image?client=kitchen`. Each distinct `client` name gets its own deck.

Without Compose:

```bash
docker run -d --name frameshuffle -p 8080:8080 \
  -v /path/to/photos:/images:ro -v frameshuffle-data:/data -v frameshuffle-cache:/cache \
  YOUR_DOCKERHUB_USER/frameshuffle:latest
```

## Configuration

Everything has a default. Deployment settings go in `.env` (see [`.env.sample`](.env.sample)); per-client
settings (intervals, time windows, filters, display profiles) go in `config/config.toml`, starting from
[`config/config.example.toml`](config/config.example.toml). The full reference is in
[`docs/configuration.md`](docs/configuration.md). Environment variables override the file.

## API

| Endpoint | |
|---|---|
| `GET /image?client=NAME` | The client's image. **Sticky:** the same image until the client's `interval_secs` has passed, then the next card in its deck. `/random` is an alias. |
| `GET /current?client=NAME` | Current image, never advances. |
| `POST /skip?client=NAME` | Advance now. |
| `GET /img/{id}` | A specific image (16+ hex characters of its hash, from `X-Image-Id`). |
| `GET /health`, `/ready` | Liveness / has-scanned-and-has-images. |
| `GET /stats`, `/metrics` | JSON report / Prometheus. |
| `POST /admin/rescan[?full=1]` | Incremental rescan (`full` rehashes everything). |
| `POST /admin/reset?client=NAME` | Discard a client's deck; its next request reshuffles. |

Image responses carry `X-Next-Refresh` (seconds until the frame should ask again), `X-Window`
(`open`/`closed`), `X-Cycle`, `X-Deck-Remaining`, `X-Deck-Size`, `X-Image-Id`, `X-Advanced`, `ETag`
and `Cache-Control: no-store`. A request with a matching `If-None-Match` gets `304`.

## How it works

**Shuffle deck.** Per client, keyed by content hash. Dealing a card marks it played; when none are left the
deck is reshuffled and `cycle` increments (a new cycle never opens with the image already on screen).

**Duplicates.** Files are identified by BLAKE3 content hash, so byte-identical copies share one card.
Near-duplicate detection (perceptual hashing) is planned; the schema already has a column for it.

**Scanning.** Startup does a fast incremental walk (path, size, mtime against the database), then hashes and
inspects only new or changed files in the background, so images become available as they are processed. Trigger
rescans with `POST /admin/rescan`, `FRAMESHUFFLE_SCAN_WATCH=true` (inotify, local disks) or
`FRAMESHUFFLE_SCAN_PERIODIC_MINUTES` (NFS/SMB). If the library suddenly looks empty (an unmounted volume), the
scan aborts rather than marking everything missing.

**Time windows.** Per client, in its timezone, optionally per weekday, may cross midnight. Outside a window the
current image is returned (or `204`) and `X-Next-Refresh` points at the next opening.

## Image processing (off by default)

With `FRAMESHUFFLE_PROCESSING=false` (the default) the server picks the image and sends the original file
byte-for-byte. That is the right setting for firmware that sizes and dithers on the device, such as
[FrameOS](https://github.com/FrameOS/frameos). The deck, filters, time windows and headers all work the same.

In a build with HEIC support (see below), HEIC/HEIF files are the one exception: most displays can't decode them,
so by default (`FRAMESHUFFLE_CONVERT_HEIC=true`) they are transcoded to a full-size JPEG with orientation applied and
no resizing or dithering.

Set `FRAMESHUFFLE_PROCESSING=true` for displays that need a ready-made image. Each client then uses a profile:
`e1004` (1600x1200 landscape JPEG, blurred padding), `e1004-dithered` (4-bit indexed PNG, Floyd-Steinberg to
Spectra 6 colours), `android-1080p`, or your own. The Spectra 6 palette values are approximations; calibrate
them against your panel.

## HEIC photos

HEIC (iPhone photos) is HEVC video-codec data, and HEVC decoders may be covered by patents in some jurisdictions.
So the **published Docker image ships without a HEIC decoder**: `.heic`/`.heif` files are indexed as
"unsupported", skipped, and the log says so (`frameshuffle_library_files{status="unsupported"}` counts them).
You have two ways to use those photos:

**1. Convert them once, locally (recommended).** [`scripts/convert-heic.sh`](scripts/convert-heic.sh) uses tools
already on your machine (`heif-convert` from libheif-examples, or ImageMagick) to write a full-size JPEG next to each
HEIC. It keeps originals by default, never overwrites, preserves timestamps, runs in parallel, and is safe to re-run:

```bash
scripts/convert-heic.sh --dry-run /path/to/photos     # see what would happen
scripts/convert-heic.sh /path/to/photos               # convert (originals kept)
scripts/convert-heic.sh --output /path/to/jpegs /path/to/photos   # or mirror the JPEGs into another folder
```

Then rescan (`POST /admin/rescan`), or let the periodic scan pick them up. (`--delete-originals` exists, asks for
confirmation, and only removes files it converted in that run.)

**2. Build an image that can decode HEIC.** HEIC support is compiled in only when you ask for it:

```bash
docker build --build-arg WITH_HEIC=1 -t frameshuffle:heic .
# or with compose: set WITH_HEIC=1 in .env, then
docker compose up -d --build
```

This adds Ubuntu's `libheif` and `libde265` packages to *your* image. Whether and how you may use or distribute an
HEVC decoder where you live is your responsibility; this is not legal advice. When running such a build the
`/stats` field `heic_support` and the metric `frameshuffle_heic_support` are `1`.

## Behind a reverse proxy (HAProxy example)

```
backend frameshuffle
    option httpchk
    http-check send meth GET uri /health
    server fs1 frameshuffle:8080 check inter 30s

frontend ...
    acl is_admin path_beg /admin /stats /metrics
    http-request deny if is_admin !{ src 192.168.0.0/16 }   # adjust to your LAN
```

## Security

Designed for a trusted home network. Set `FRAMESHUFFLE_TOKEN` and `FRAMESHUFFLE_ADMIN_TOKEN` if the port is
reachable by anything you don't fully trust, and prefer the `Authorization: Bearer` header over `?token=` (query
strings end up in logs). The photo folder is mounted read-only and the container runs unprivileged with a
read-only root filesystem. Report vulnerabilities as described in [SECURITY.md](SECURITY.md).

If your photos aren't world-readable, either relax the permissions or set `user:` in the compose file to a uid
that can read them (and make sure that uid can write the data and cache volumes).

## Metrics

`/metrics` and `/stats` report, among others: files found / ok / pending / unsupported / corrupt / missing, unique
images, duplicate files, orientation and format mix, scan durations and last-scan new/changed/removed counts,
per-client deck size, remaining, cycle, served total and last-seen age, conversion cache hits/misses/size and
latency, HTTP requests by route and status, 304s, window-closed responses, and database size.

## Known limitations

- When converting (processing on, or HEIC transcoding), embedded ICC profiles are ignored, so Display P3 images
  from phones come out slightly less saturated than on the phone.
- With processing off, a file that passes the header check but is damaged further in is served as-is; only
  conversion detects and quarantines those.
- Renaming or moving a file keeps its place in the deck (same hash), but editing it counts as a new image.
- EXIF capture dates are read as wall-clock time, so date filters are accurate to about a day.
- inotify needs a local filesystem and enough `fs.inotify.max_user_watches` for your folder count.

## Building from source

```bash
sudo apt-get install pkg-config zlib1g-dev libheif-dev libheif-plugin-libde265   # libheif >= 1.17
cargo build --release                         # with HEIC support (the default cargo feature)
cargo build --release --no-default-features   # without HEIC / libheif (what the official image uses)
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for development and release notes, and [CHANGELOG.md](CHANGELOG.md).

## License

[MIT](LICENSE). The Docker image also contains Ubuntu packages under their own licenses. The official image contains no
HEIC/HEVC decoder; a `WITH_HEIC=1` build adds libheif and libde265 (LGPL).
