# Security policy

## Reporting a vulnerability

Please report security issues privately through GitHub's **Security → Report a vulnerability**
form on this repository rather than opening a public issue. Include what you found, how to reproduce
it, and the version (`frameshuffle --version` or the image tag). I'll acknowledge reports as soon as I can.

## Hardening notes for operators

frameshuffle is designed for a trusted home network, behind a reverse proxy.

- It serves only files under the configured library directory, which is mounted read-only.
- Set `FRAMESHUFFLE_TOKEN` and `FRAMESHUFFLE_ADMIN_TOKEN` if the port is reachable by anything you
  don't fully trust. Prefer the `Authorization: Bearer` header; a `?token=` query parameter can end up
  in proxy and access logs.
- Keep `/admin/*`, `/stats` and `/metrics` off the public internet (see the HAProxy example in the README).
- The container runs as an unprivileged user with a read-only root filesystem.
- Only the latest release is supported.
