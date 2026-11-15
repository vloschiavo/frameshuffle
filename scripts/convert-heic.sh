#!/usr/bin/env bash
# Convert HEIC/HEIF photos to JPEG using tools already installed on YOUR machine.
#
# frameshuffle's official Docker image deliberately ships without a HEIC decoder (HEIC is HEVC video,
# which is covered by patents in some jurisdictions). Converting your own library once, locally, is the
# simplest way to use those photos with it. See "HEIC photos" in the README.
#
#   scripts/convert-heic.sh [options] SOURCE_DIR
#
# Safe by default: originals are kept, existing JPEGs are never overwritten, and files are written
# atomically, so it is fine to interrupt and re-run.

set -euo pipefail

QUALITY=92
JOBS=""
OUTDIR=""
DELETE=0
ASSUME_YES=0
DRY_RUN=0

usage() {
  cat <<'EOF'
Usage: convert-heic.sh [options] SOURCE_DIR

Converts every .heic/.heif/.hif file under SOURCE_DIR to a full-size JPEG (no resizing).

Options:
  -o, --output DIR        write JPEGs into DIR, mirroring the folder structure
                          (default: next to each original, e.g. IMG_0001.HEIC -> IMG_0001.jpg)
  -q, --quality N         JPEG quality 1-100 (default: 92)
  -j, --jobs N            parallel conversions (default: number of CPUs)
  -d, --delete-originals  delete each original once its JPEG has been written and checked.
                          Only files converted in this run are deleted. Asks for confirmation.
  -y, --yes               don't ask for confirmation
  -n, --dry-run           show what would happen, change nothing
  -h, --help              show this help

Needs one of: heif-convert (Debian/Ubuntu: libheif-examples, macOS: brew install libheif),
or ImageMagick built with HEIC support (magick / convert).
Photo metadata (EXIF) is copied by those tools; spot-check a converted file. File modification
times are preserved. Files are skipped, never overwritten, if the target JPEG already exists.
EOF
}

die() { echo "convert-heic: $*" >&2; exit 2; }

# ---------------------------------------------------------------------------------------------
# Worker mode: convert exactly one file. Re-invoked by xargs for parallelism; configuration comes
# in through the environment. Prints one "STATUS<TAB>path" line and always exits 0.
# ---------------------------------------------------------------------------------------------
convert_one() {
  local in="$1" rel target dir work out status="converted"
  rel="${in#"$CH_SRC"/}"
  if [[ -n "$CH_OUT" ]]; then
    target="$CH_OUT/${rel%.*}.jpg"
  else
    target="${in%.*}.jpg"
  fi
  dir="$(dirname "$target")"

  if [[ -s "$target" ]]; then
    printf 'skipped (exists)\t%s\n' "$in"
    return 0
  fi
  if [[ "$CH_DRY" == 1 ]]; then
    printf 'would convert\t%s -> %s\n' "$in" "$target"
    return 0
  fi

  mkdir -p "$dir"
  # Work in a hidden temp dir next to the target (same filesystem, so the final mv is atomic;
  # dot-directories are ignored by frameshuffle's scanner if it happens to be watching).
  work="$(mktemp -d "$dir/.convert-heic.XXXXXX")" || { printf 'failed (temp dir)\t%s\n' "$in"; return 0; }
  # shellcheck disable=SC2064  # expand $work now, on purpose
  trap "rm -rf '$work'" RETURN

  out="$work/out.jpg"
  local rc=0
  case "$CH_TOOL" in
    heif-convert) heif-convert --quiet -q "$CH_QUALITY" "$in" "$out" >/dev/null 2>"$work/err" || rc=$? ;;
    magick)       magick "${in}[0]" -quality "$CH_QUALITY" "$out" 2>"$work/err" || rc=$? ;;
    convert)      convert "${in}[0]" -quality "$CH_QUALITY" "$out" 2>"$work/err" || rc=$? ;;
  esac
  if [[ $rc -ne 0 ]]; then
    printf 'failed (%s exit %s: %s)\t%s\n' "$CH_TOOL" "$rc" "$(head -c 200 "$work/err" 2>/dev/null | tr '\n' ' ')" "$in"
    return 0
  fi

  # Files with several images (bursts, live photos) make heif-convert write out-1.jpg, out-2.jpg, ...
  if [[ ! -s "$out" ]]; then
    local first
    first="$(find "$work" -maxdepth 1 -name 'out-*.jpg' | sort -V | head -n1)"
    if [[ -n "$first" ]]; then
      mv -f "$first" "$out"
      status="converted (first of several images)"
    fi
  fi

  # Verify: non-empty and a JPEG (starts with FF D8).
  if [[ ! -s "$out" ]] || [[ "$(head -c 2 "$out" | od -An -tx1 | tr -d ' \n')" != "ffd8" ]]; then
    printf 'failed (no valid JPEG produced)\t%s\n' "$in"
    return 0
  fi

  touch -r "$in" "$out"           # keep the original timestamp (frameshuffle date filters, file managers)
  mv -n "$out" "$target"          # never clobber
  if [[ ! -s "$target" ]]; then
    printf 'failed (could not write target)\t%s\n' "$in"
    return 0
  fi
  if [[ "$CH_DELETE" == 1 ]]; then
    rm -f -- "$in" && status="$status, original deleted"
  fi
  printf '%s\t%s\n' "$status" "$in"
}

if [[ "${1:-}" == "__convert_one" ]]; then
  convert_one "$2"
  exit 0
fi

# ---------------------------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------------------------
SRC=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    -o|--output)           [[ $# -ge 2 ]] || die "$1 needs a value"; OUTDIR="$2"; shift 2 ;;
    -q|--quality)          [[ $# -ge 2 ]] || die "$1 needs a value"; QUALITY="$2"; shift 2 ;;
    -j|--jobs)             [[ $# -ge 2 ]] || die "$1 needs a value"; JOBS="$2"; shift 2 ;;
    -d|--delete-originals) DELETE=1; shift ;;
    -y|--yes)              ASSUME_YES=1; shift ;;
    -n|--dry-run)          DRY_RUN=1; shift ;;
    -h|--help)             usage; exit 0 ;;
    -*)                    usage >&2; die "unknown option: $1" ;;
    *)                     [[ -z "$SRC" ]] || die "only one SOURCE_DIR is allowed"; SRC="$1"; shift ;;
  esac
done

[[ -n "$SRC" ]] || { usage >&2; exit 2; }
[[ -d "$SRC" ]] || die "not a directory: $SRC"
if ! [[ "$QUALITY" =~ ^[0-9]+$ ]] || (( QUALITY < 1 || QUALITY > 100 )); then die "quality must be 1-100"; fi
if [[ -z "$JOBS" ]]; then JOBS="$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 2)"; fi
if ! [[ "$JOBS" =~ ^[0-9]+$ ]] || (( JOBS < 1 )); then die "jobs must be a positive number"; fi

TOOL=""
if   command -v heif-convert >/dev/null 2>&1; then TOOL="heif-convert"
elif command -v magick       >/dev/null 2>&1; then TOOL="magick"
elif command -v convert      >/dev/null 2>&1; then TOOL="convert"
else die "need heif-convert (libheif-examples) or ImageMagick with HEIC support installed"; fi

SRC="$(cd "$SRC" && pwd)"
if [[ -n "$OUTDIR" ]]; then mkdir -p "$OUTDIR"; OUTDIR="$(cd "$OUTDIR" && pwd)"; fi

mapfile -d '' FILES < <(
  find "$SRC" -mindepth 1 \( -type d \( -name '@eaDir' -o -name '#recycle' -o -name '.*' \) -prune \) -o \
       -type f \( -iname '*.heic' -o -iname '*.heif' -o -iname '*.hif' \) -print0
)
TOTAL=${#FILES[@]}
if (( TOTAL == 0 )); then echo "No HEIC/HEIF files found under $SRC"; exit 0; fi

echo "Found $TOTAL HEIC/HEIF file(s) under $SRC; converting with $TOOL (quality $QUALITY, $JOBS parallel job(s))."
if (( DELETE == 1 && DRY_RUN == 0 )); then
  echo "WARNING: --delete-originals will remove each .heic after its JPEG is written and checked."
  if (( ASSUME_YES == 0 )); then
    [[ -t 0 ]] || die "refusing to delete originals without a terminal; pass --yes to confirm"
    read -r -p "Type 'yes' to continue: " answer
    [[ "$answer" == "yes" ]] || { echo "Aborted."; exit 1; }
  fi
fi

export CH_SRC="$SRC" CH_OUT="$OUTDIR" CH_QUALITY="$QUALITY" CH_TOOL="$TOOL" CH_DRY="$DRY_RUN" CH_DELETE="$DELETE"
LOG="$(mktemp)"
trap 'rm -f "$LOG"' EXIT
SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"

START=$SECONDS
printf '%s\0' "${FILES[@]}" | xargs -0 -n 1 -P "$JOBS" "$SELF" __convert_one | tee "$LOG"

converted=$(grep -c '^converted' "$LOG" || true)
skipped=$(grep -c '^skipped' "$LOG" || true)
would=$(grep -c '^would convert' "$LOG" || true)
failed=$(grep -c '^failed' "$LOG" || true)
echo
if (( DRY_RUN == 1 )); then
  echo "Dry run: $would to convert, $skipped already done."
else
  echo "Done in $((SECONDS - START))s: $converted converted, $skipped skipped, $failed failed (of $TOTAL)."
fi
(( failed == 0 ))
