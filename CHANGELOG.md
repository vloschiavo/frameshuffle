# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-11-11

First release.

### Added
- Per-client shuffled decks: no repeats until every image has been shown, persisted in SQLite.
- Exact-duplicate suppression by BLAKE3 content hash; schema ready for perceptual hashes.
- Incremental startup scan, `POST /admin/rescan`, optional inotify watcher and periodic rescan.
- Sticky `/image` endpoint (same image until the client's interval elapses), plus `/current`,
  `/skip`, `/img/{id}`.
- Per-client time windows and `X-Next-Refresh`; ETag and `304` support.
- Per-client filters: orientation, minimum size, include/exclude paths, EXIF date range.
- Optional image processing (off by default): resize, blurred-background padding, contrast and
  saturation, Floyd-Steinberg dithering to a palette (Spectra 6), JPEG/PNG output, on-disk cache.
- HEIC/HEIF support, **opt-in at build time** (`WITH_HEIC=1` / cargo feature `heic`). The published Docker
  image ships without a HEIC decoder; HEIC files are reported as unsupported with instructions. With processing
  off, a HEIC-enabled build transcodes HEIC to full-size JPEG.
- `scripts/convert-heic.sh`: one-time, local HEIC-to-JPEG conversion using the user's own tools.
- `/health`, `/ready`, `/stats` (JSON) and `/metrics` (Prometheus).
- Configuration by TOML file and/or `FRAMESHUFFLE_*` environment variables.
- Multi-arch (amd64, arm64) Docker image.
