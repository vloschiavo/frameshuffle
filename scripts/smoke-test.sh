#!/usr/bin/env bash
# End-to-end check against a running frameshuffle.
#   scripts/smoke-test.sh [BASE_URL] [TOKEN]
# Set EXPECT_HEIC=1 (build with HEIC) or EXPECT_HEIC=0 (default build) to also check HEIC handling;
# the test library then needs HEIC files (scripts/make-test-images.py DIR --heic).
# Verifies: health/readiness, that a full cycle of /skip shows every unique image exactly once,
# sticky /image behaviour, 304 on a matching ETag, and that key metrics are exported.
# shellcheck disable=SC2016,SC2034  # check() takes single-quoted expressions that it evals; the vars are used there
set -euo pipefail

BASE="${1:-http://127.0.0.1:8080}"
TOKEN="${2:-}"
Q=""; [[ -n "$TOKEN" ]] && Q="&token=$TOKEN"
CLIENT="smoke-$$"
pass=0; fail=0
ok()   { printf '  \033[32mPASS\033[0m %s\n' "$1"; pass=$((pass+1)); }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=$((fail+1)); }
check(){ if eval "$2"; then ok "$1"; else bad "$1"; fi; }
hdr()  { tr -d '\r' | awk -F': ' -v k="$1" 'tolower($1)==tolower(k){print $2}' | head -1; }

echo "== $BASE"
check "/health is 200" '[[ $(curl -s -o /dev/null -w "%{http_code}" "$BASE/health") == 200 ]]'

echo "waiting for the first scan..."
for _ in $(seq 1 120); do
  [[ $(curl -s -o /dev/null -w "%{http_code}" "$BASE/ready") == 200 ]] && break
  sleep 1
done
check "/ready is 200" '[[ $(curl -s -o /dev/null -w "%{http_code}" "$BASE/ready") == 200 ]]'

# Wait for hashing to finish so the unique count is final.
for _ in $(seq 1 600); do
  pending=$(curl -fsS "$BASE/metrics?${Q#&}" | awk '/^frameshuffle_library_files\{status="pending"\}/{print $2}')
  [[ "${pending:-0}" == 0 ]] && break
  sleep 1
done
metrics=$(curl -fsS "$BASE/metrics?${Q#&}")
unique=$(awk '/^frameshuffle_library_unique_images /{print $2}' <<<"$metrics")
echo "unique images: $unique"
check "library has images" '[[ ${unique:-0} -gt 0 ]]'

# One full cycle with /skip: every image exactly once.
ids=()
for _ in $(seq 1 "$unique"); do
  id=$(curl -fsS -X POST -D - -o /dev/null "$BASE/skip?client=$CLIENT$Q" | hdr x-image-id)
  ids+=("$id")
done
dupes=$(printf '%s\n' "${ids[@]}" | sort | uniq -d | wc -l)
check "a full cycle ($unique skips) shows no repeats" '[[ $dupes -eq 0 ]]'
cycle=$(curl -fsS -D - -o /dev/null "$BASE/current?client=$CLIENT$Q" | hdr x-cycle)
check "still in cycle 1 after one pass" '[[ $cycle == 1 ]]'
next=$(curl -fsS -X POST -D - -o /dev/null "$BASE/skip?client=$CLIENT$Q" | hdr x-cycle)
check "the next pull starts cycle 2" '[[ $next == 2 ]]'

# Headers and conditional requests.
H=$(curl -fsS -D - -o /tmp/smoke.img "$BASE/current?client=$CLIENT$Q")
check "Content-Type is an image" 'grep -qi "^content-type: image/" <<<"$H"'
check "Cache-Control: no-store" 'grep -qi "^cache-control: no-store" <<<"$H"'
etag=$(hdr etag <<<"$H")
code=$(curl -s -o /dev/null -w '%{http_code}' -H "If-None-Match: $etag" "$BASE/current?client=$CLIENT$Q")
check "matching ETag -> 304" '[[ $code == 304 ]]'
if command -v identify >/dev/null; then
  dims=$(identify -format '%wx%h' /tmp/smoke.img)
  echo "  dimensions: $dims"
fi

# Sticky: /image does not advance inside the interval (default client = 3600 s).
a=$(curl -fsS -D - -o /dev/null "$BASE/image?client=sticky-$$$Q" | hdr x-image-id)
b=$(curl -fsS -D - -o /dev/null "$BASE/image?client=sticky-$$$Q" | hdr x-image-id)
check "/image is sticky within the interval" '[[ -n $a && $a == $b ]]'
nr=$(curl -fsS -D - -o /dev/null "$BASE/image?client=sticky-$$$Q" | hdr x-next-refresh)
check "X-Next-Refresh is present" '[[ -n $nr ]]'

metrics=$(curl -fsS "$BASE/metrics?${Q#&}")   # refresh after the traffic above
for m in frameshuffle_library_unique_images frameshuffle_library_files frameshuffle_scan_last_duration_seconds \
         frameshuffle_client_deck_remaining frameshuffle_render_cache_total frameshuffle_http_requests_total; do
  check "metric $m exported" 'grep -q "^$m" <<<"$metrics"'
done

if [[ -n "${EXPECT_HEIC:-}" ]]; then
  support=$(awk '/^frameshuffle_heic_support /{print $2}' <<<"$metrics")
  has_heic=$(grep -c '^frameshuffle_library_format{format="heic"}' <<<"$metrics" || true)
  unsupported=$(awk '/^frameshuffle_library_files\{status="unsupported"\}/{print $2}' <<<"$metrics")
  if [[ "$EXPECT_HEIC" == 1 ]]; then
    check "build reports HEIC support" '[[ $support == 1 ]]'
    check "HEIC files are indexed and served" '[[ $has_heic -ge 1 ]]'
    id=$(curl -fsS -D - -o /dev/null "$BASE/skip?client=heic-$$$Q" -X POST | hdr x-image-id)
    check "a HEIC-containing library serves JPEG" '[[ -n $id ]]'
  else
    check "build reports no HEIC support" '[[ $support == 0 ]]'
    check "HEIC files are not served" '[[ $has_heic -eq 0 ]]'
    check "HEIC files are counted as unsupported" '[[ ${unsupported:-0} -ge 1 ]]'
  fi
fi

echo; echo "passed=$pass failed=$fail"
[[ $fail -eq 0 ]]
