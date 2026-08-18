# fs-caching-server

An async Rust HTTP proxy that caches matching successful `GET` responses on the
local filesystem.  Primarily useful as, and designed to be, a caching proxy for
binary packages, specifically pkgsrc.

It aims to be compatible with the behavior of the original nodejs
`fs-caching-server` client:
<https://github.com/bahamas10/node-fs-caching-server>

## Run

```sh
cargo run --release -- \
  --cache-dir /data \
  --host 127.0.0.1 \
  --port 8080 \
  --url https://pkgsrc.smartos.org/
```

Configuration is available as command-line flags or environment variables:

| Flag | Environment | Default |
| --- | --- | --- |
| `--cache-dir` | `FS_CACHE_DIR` | `.` |
| `--debug` | `FS_CACHE_DEBUG` | disabled |
| `--host` | `FS_CACHE_HOST` | `0.0.0.0` |
| `--port` | `FS_CACHE_PORT` | `8080` |
| `--regex` | `FS_CACHE_REGEX` | common web assets and archives |
| `--url` | `FS_CACHE_URL` | required |
