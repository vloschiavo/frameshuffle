# Contributing

Bug reports and pull requests are welcome.

## Development

```bash
# Debian/Ubuntu build dependencies (libheif >= 1.17 for HEIC support)
sudo apt-get install pkg-config zlib1g-dev libheif-dev libheif-plugin-libde265

cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
# The published image is built WITHOUT HEIC, so keep that configuration healthy too:
cargo clippy --no-default-features --all-targets -- -D warnings && cargo test --no-default-features
```

CI runs the same checks, then builds the Docker image twice (default, and `WITH_HEIC=1`) and runs
`scripts/smoke-test.sh` against each, with `EXPECT_HEIC` set accordingly. It also asserts that the default image
contains no libheif/libde265. Shell scripts should pass `shellcheck`.
To try the smoke test locally:

```bash
pip install pillow pillow-heif && python3 scripts/make-test-images.py /tmp/photos --heic
FRAMESHUFFLE_LIBRARY_DIR=/tmp/photos FRAMESHUFFLE_DB_PATH=/tmp/fs.db FRAMESHUFFLE_CACHE_DIR=/tmp/fs-cache \
  FRAMESHUFFLE_BIND=127.0.0.1:8080 cargo run --release &
EXPECT_HEIC=1 scripts/smoke-test.sh http://127.0.0.1:8080   # cargo run builds WITH HEIC by default
```

## Releasing (maintainers)

1. Bump `version` in `Cargo.toml` (and run `cargo build` so `Cargo.lock` follows).
2. Move the "Unreleased" notes in `CHANGELOG.md` under the new version.
3. Commit, then `git tag vX.Y.Z && git push --tags`.

The tag triggers `.github/workflows/docker.yml`, which checks the tag matches `Cargo.toml`, builds
amd64 and arm64 images, publishes `:X.Y.Z`, `:X.Y` and `:latest` to Docker Hub, and refreshes the
Docker Hub description from `README.md`.
