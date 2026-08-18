# fs-caching-server

An async Rust HTTP proxy that caches matching successful `GET` responses on the
local filesystem.  Primarily useful as, and designed to be, a caching proxy for
binary packages, specifically pkgsrc.

It aims to be broadly compatible with the behavior of the original nodejs
`fs-caching-server` client:
<https://github.com/bahamas10/node-fs-caching-server>

## Install

```sh
cargo install --git https://github.com/jperkin/fs-caching-server
```

## Run

Cache all requests to `/data`:

```sh
fs-caching-server \
  --cache-dir /data \
  --host 127.0.0.1 \
  --url https://pkgsrc.smartos.org/
```

Limit cache to just a specific package directory (other requests will pass
through and not be stored locally):

```sh
fs-caching-server \
  -d /data \
  -H 127.0.0.1 \
  -r '^/packages/SmartOS/2025Q4/x86_64/All/.*\.(gz|tgz|zst)$' \
  -U https://pkgsrc.smartos.org/
```

Configuration is available as command-line flags or environment variables:

| Flag | Environment | Default |
| --- | --- | --- |
| `--cache-dir` | `FS_CACHE_DIR` | `.` |
| `--debug` | `FS_CACHE_DEBUG` | disabled |
| `--host` | `FS_CACHE_HOST` | `0.0.0.0` |
| `--port` | `FS_CACHE_PORT` | `8080` |
| `--regex` | `FS_CACHE_REGEX` | `\.(gz|tgz|zst)$` |
| `--url` | `FS_CACHE_URL` | required |
