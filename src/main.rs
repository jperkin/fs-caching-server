/*
 * Copyright (c) 2026 Jonathan Perkin <jonathan@perkin.org.uk>
 *
 * Permission to use, copy, modify, and distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 */

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use anyhow::{Context, Result};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderName, HeaderValue, Method, Request, StatusCode, header},
    response::Response,
};
use clap::Parser;
use percent_encoding::percent_decode_str;
use regex::Regex;
use reqwest::{Client, Url};
use tokio::{
    fs,
    sync::{Mutex, Notify},
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

const DEFAULT_REGEX: &str = r"\.(gz|tgz|zst)$";
const SKIP_HEADERS: [&str; 3] = ["date", "server", "host"];

#[derive(Parser, Debug, Clone)]
#[command(
    name = "fs-caching-server",
    version,
    about = "An HTTP caching proxy backed by the local filesystem"
)]
struct Config {
    /// Directory where cached response bodies are stored.
    #[arg(short = 'c', long, env = "FS_CACHE_DIR", default_value = ".")]
    cache_dir: PathBuf,
    /// Enable debug logging
    #[arg(short, long, env = "FS_CACHE_DEBUG")]
    debug: bool,
    /// Address to listen on.
    #[arg(short = 'H', long, env = "FS_CACHE_HOST", default_value = "0.0.0.0")]
    host: String,
    #[arg(short, long, env = "FS_CACHE_PORT", default_value_t = 8080)]
    port: u16,
    /// Regular expression matched against decoded request paths.
    #[arg(short, long, env = "FS_CACHE_REGEX", default_value = DEFAULT_REGEX)]
    regex: String,
    /// Backend HTTP(S) URL.
    #[arg(short = 'U', long, env = "FS_CACHE_URL")]
    url: String,
}

#[derive(Clone)]
struct AppState {
    config: Arc<RuntimeConfig>,
    client: Client,
    in_progress: Arc<Mutex<HashMap<PathBuf, Arc<Notify>>>>,
}

struct RuntimeConfig {
    cache_dir: PathBuf,
    backend: Url,
    matcher: Regex,
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::parse();
    let log_filter = if config.debug {
        "info,fs_caching_server=debug"
    } else {
        "info"
    };
    tracing_subscriber::fmt()
        .with_target(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new(log_filter))
        .init();
    let backend = Url::parse(&config.url).context("backend URL is invalid")?;
    anyhow::ensure!(
        matches!(backend.scheme(), "http" | "https"),
        "backend URL must use http or https"
    );
    let matcher = Regex::new(&config.regex).context("cache regex is invalid")?;
    fs::create_dir_all(&config.cache_dir)
        .await
        .context("creating cache directory")?;
    let state = AppState {
        config: Arc::new(RuntimeConfig {
            cache_dir: config.cache_dir,
            backend,
            matcher,
        }),
        client: Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        in_progress: Default::default(),
    };
    let app = Router::new().fallback(proxy).with_state(state);
    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .context("invalid listen address")?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        error!(%error, "failed to listen for shutdown signal");
    }
    info!("shutting down");
}

async fn proxy(State(state): State<AppState>, request: Request<Body>) -> Response {
    let id = Uuid::new_v4();
    let method = request.method().clone();
    let path = match decoded_path(request.uri().path()) {
        Ok(path) => path,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let cacheable =
        matches!(method, Method::GET | Method::HEAD) && state.config.matcher.is_match(&path);
    if !cacheable {
        return forward(&state, request, id, "passthrough").await;
    }
    let cache_path = match cache_path(&state.config.cache_dir, &path) {
        Ok(path) => path,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    match fs::metadata(&cache_path).await {
        Ok(metadata) if metadata.is_dir() => {
            debug!(%id, %method, path, cache = "invalid-directory", "request complete");
            return StatusCode::BAD_REQUEST.into_response();
        }
        Ok(metadata) => {
            debug!(%id, %method, path, cache = "hit", status = 200, "request complete");
            return cached_response(&cache_path, metadata, &method, request.headers()).await;
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            warn!(%error, "cache stat failed")
        }
        _ => {}
    }
    if method == Method::HEAD {
        return forward(&state, request, id, "miss-head").await;
    }

    let (notify, leader) = {
        let mut pending = state.in_progress.lock().await;
        match pending.get(&cache_path) {
            Some(waiter) => (waiter.clone(), false),
            None => {
                let waiter = Arc::new(Notify::new());
                pending.insert(cache_path.clone(), waiter.clone());
                (waiter, true)
            }
        }
    };
    if !leader {
        notify.notified().await;
        return match fs::metadata(&cache_path).await {
            Ok(metadata) if metadata.is_file() => {
                cached_response(&cache_path, metadata, &method, request.headers()).await
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    }
    let result = fetch_and_cache(&state, request, &cache_path, id, &path).await;
    if let Some(waiter) = state.in_progress.lock().await.remove(&cache_path) {
        waiter.notify_waiters();
    }
    result
}

async fn fetch_and_cache(
    state: &AppState,
    request: Request<Body>,
    cache_path: &Path,
    id: Uuid,
    path: &str,
) -> Response {
    let response = match send_backend(state, request).await {
        Ok(response) => response,
        Err(error) => {
            error!(%error, "backend request failed");
            debug!(%id, %path, cache = "miss", outcome = "backend-error", "request complete");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let status = response.status();
    if !(200..300).contains(&status.as_u16()) {
        debug!(%id, %path, cache = "miss", backend_status = status.as_u16(), cached = false, "request complete");
        return backend_response(response, false).await;
    }
    let headers = response.headers().clone();
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(error) => {
            error!(%error, "reading backend response failed");
            debug!(%id, %path, cache = "miss", backend_status = status.as_u16(), outcome = "backend-read-error", "request complete");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    if let Some(parent) = cache_path.parent()
        && let Err(error) = fs::create_dir_all(parent).await
    {
        error!(%error, "creating cache parent failed");
        debug!(%id, %path, cache = "miss", backend_status = status.as_u16(), outcome = "cache-directory-error", "request complete");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let temporary = cache_path.with_file_name(format!(".{}.in-progress", Uuid::new_v4()));
    if let Err(error) = fs::write(&temporary, &body).await {
        error!(%error, "writing cache failed");
        debug!(%id, %path, cache = "miss", backend_status = status.as_u16(), outcome = "cache-write-error", "request complete");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if let Err(error) = fs::rename(&temporary, cache_path).await {
        if let Err(cleanup_error) = fs::remove_file(&temporary).await {
            warn!(%cleanup_error, "failed to remove temporary cache file");
        }
        error!(%error, "installing cache file failed");
        debug!(%id, %path, cache = "miss", backend_status = status.as_u16(), outcome = "cache-install-error", "request complete");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    debug!(%id, %path, cache = "miss", backend_status = status.as_u16(), cached = true, "request complete");
    response_with_headers(status, body, &headers)
}

async fn forward(state: &AppState, request: Request<Body>, id: Uuid, cache: &str) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    match send_backend(state, request).await {
        Ok(response) => {
            let status = response.status().as_u16();
            debug!(%id, %method, %path, cache, backend_status = status, "request complete");
            backend_response(response, false).await
        }
        Err(error) => {
            error!(%error, "backend request failed");
            debug!(%id, %method, %path, cache, outcome = "backend-error", "request complete");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

async fn send_backend(state: &AppState, request: Request<Body>) -> Result<reqwest::Response> {
    let uri = request.uri();
    let mut backend = state.config.backend.clone();
    let base = backend.path().trim_end_matches('/');
    backend.set_path(&format!("{}{}", base, uri.path()));
    backend.set_query(uri.query());
    let mut builder = state.client.request(request.method().clone(), backend);
    for (name, value) in request.headers() {
        if !SKIP_HEADERS.contains(&name.as_str()) {
            builder = builder.header(name, value);
        }
    }
    let body = to_bytes(request.into_body(), usize::MAX).await?;
    Ok(builder.body(body).send().await?)
}

async fn backend_response(response: reqwest::Response, _cached: bool) -> Response {
    let status = match StatusCode::from_u16(response.status().as_u16()) {
        Ok(status) => status,
        Err(error) => {
            error!(%error, "backend returned an invalid HTTP status");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let headers = response.headers().clone();
    match response.bytes().await {
        Ok(body) => response_with_headers(status, body, &headers),
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

async fn cached_response(
    path: &Path,
    metadata: std::fs::Metadata,
    method: &Method,
    request_headers: &http::HeaderMap,
) -> Response {
    let modified = match metadata.modified() {
        Ok(modified) => modified,
        Err(error) => {
            error!(%error, "reading cache modification time failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let millis = match modified.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => duration.as_millis(),
        Err(error) => {
            error!(%error, "cache modification time predates Unix epoch");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let etag = format!("\"{}-{}\"", metadata.len(), millis);
    let content_type = mime_guess::from_path(path).first_or_octet_stream();
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::OK;
    let response_headers = response.headers_mut();
    for (name, value) in [
        (header::LAST_MODIFIED, httpdate::fmt_http_date(modified)),
        (header::CONTENT_TYPE, content_type.to_string()),
        (header::ETAG, etag.clone()),
        (header::CONTENT_LENGTH, metadata.len().to_string()),
    ] {
        let value = match HeaderValue::try_from(value) {
            Ok(value) => value,
            Err(error) => {
                error!(%error, ?name, "building cache response header failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        response_headers.insert(name, value);
    }
    if request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|x| x.to_str().ok())
        == Some(etag.as_str())
    {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        return response;
    }
    if method == Method::HEAD {
        return response;
    }
    match fs::read(path).await {
        Ok(body) => {
            *response.body_mut() = Body::from(body);
            response
        }
        Err(error) => {
            error!(%error, "reading cache failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn response_with_headers(
    status: StatusCode,
    body: impl Into<Body>,
    headers: &reqwest::header::HeaderMap,
) -> Response {
    let mut response = Response::new(body.into());
    *response.status_mut() = status;
    let response_headers = response.headers_mut();
    for (name, value) in headers {
        if SKIP_HEADERS.contains(&name.as_str()) {
            continue;
        }
        let name = match HeaderName::try_from(name.as_str()) {
            Ok(name) => name,
            Err(error) => {
                warn!(%error, header = %name, "ignoring invalid backend response header name");
                continue;
            }
        };
        let value = match HeaderValue::try_from(value.as_bytes()) {
            Ok(value) => value,
            Err(error) => {
                warn!(%error, %name, "ignoring invalid backend response header value");
                continue;
            }
        };
        response_headers.append(name, value);
    }
    response
}
fn decoded_path(path: &str) -> Result<String> {
    Ok(percent_decode_str(path)
        .decode_utf8()
        .context("request path is not valid UTF-8 after percent decoding")?
        .into_owned())
}
fn cache_path(root: &Path, path: &str) -> Result<PathBuf> {
    let mut result = root.to_path_buf();
    for component in path.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            anyhow::bail!("request path escapes cache directory");
        }
        result.push(component);
    }
    Ok(result)
}

trait IntoResponse {
    fn into_response(self) -> Response;
}
impl IntoResponse for StatusCode {
    fn into_response(self) -> Response {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = self;
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_paths_stay_under_the_cache_root() -> Result<()> {
        let root = tempfile::tempdir()?;
        assert!(matches!(
            cache_path(root.path(), "/a/b.png"),
            Ok(path) if path == root.path().join("a/b.png")
        ));
        assert!(cache_path(root.path(), "/a/../secret.png").is_err());
        assert!(cache_path(root.path(), "/a/%2e%2e/secret.png").is_ok());
        Ok(())
    }

    #[test]
    fn percent_encoded_paths_are_decoded_before_matching() -> Result<()> {
        assert!(matches!(
            decoded_path("/images/hello%20world.png"),
            Ok(path) if path == "/images/hello world.png"
        ));
        assert!(decoded_path("/%ff.png").is_err());
        Ok(())
    }
}
