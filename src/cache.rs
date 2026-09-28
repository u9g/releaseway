//! The cache: `GET /<host>/<path>` answers with `https://<host>/<path>`,
//! from the bucket when it has it, and otherwise fetched once, put in the
//! bucket, and answered from there.
//!
//! Only the configured hosts are cached, and a file the bucket does not have
//! yet is only fetched for a caller holding the token, so it is not an open
//! proxy that anyone can fill the bucket through. What is there already is
//! anybody's, as it is upstream.
//!
//! Nothing is ever fetched again once it is in the bucket. That suits what
//! it is for: a build tool that names every download by its hash and checks
//! it (Bazel's `sha256`, a lockfile's integrity) fails a file that changed
//! upstream wherever it came from, so the first copy is the one to keep.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use tokio::io::AsyncWriteExt;

/// GitHub's download hosts: a repository's archives and releases, and
/// where GitHub redirects them. What is cached when nothing else is said.
pub const GITHUB: &[&str] = &[
    "github.com",
    "codeload.github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// How long a signed request to the bucket stays good for.
const SIGNED_FOR: Duration = Duration::from_secs(600);

/// An S3 bucket (R2, S3, anything that speaks S3's API and signing).
pub struct BucketConfig {
    /// e.g. `https://<account id>.r2.cloudflarestorage.com`.
    pub endpoint: String,
    pub name: String,
    /// `auto` for R2.
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

pub struct Config {
    pub bucket: BucketConfig,
    /// The hosts cached; a redirect to any other is refused.
    pub hosts: Vec<String>,
    /// Without one, nothing new is fetched at all.
    pub token: Option<String>,
    /// Where a download waits between upstream and the bucket.
    pub scratch: PathBuf,
    /// The largest file fetched.
    pub most_bytes: u64,
    /// In tests, the stand-in every host is served by, as
    /// `<origin>/<host>/<path>`; `https://<host>` otherwise.
    pub origin: Option<String>,
}

pub struct Cache {
    /// Upstream: follows redirects, but only to the configured hosts.
    upstream: reqwest::Client,
    /// The bucket.
    http: reqwest::Client,
    bucket: Bucket,
    credentials: Credentials,
    hosts: Vec<String>,
    token: Option<String>,
    scratch: PathBuf,
    most_bytes: u64,
    /// Names each download's scratch file.
    downloads: AtomicU64,
    origin: Option<String>,
}

impl Cache {
    pub fn new(config: Config) -> Result<Cache, String> {
        // reqwest is built without a crypto provider of its own, so rustls
        // has to be told which, once per process.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let endpoint = config
            .bucket
            .endpoint
            .parse()
            .map_err(|e| format!("the bucket's endpoint {}: {e}", config.bucket.endpoint))?;
        let bucket = Bucket::new(
            endpoint,
            UrlStyle::Path,
            config.bucket.name,
            config.bucket.region,
        )
        .map_err(|e| format!("the bucket: {e}"))?;
        let hosts = config.hosts.clone();
        let stand_in = config.origin.clone();
        let redirects = reqwest::redirect::Policy::custom(move |attempt| {
            let allowed = attempt.url().host_str().is_some_and(|host| {
                hosts.iter().any(|h| h == host)
                    || stand_in
                        .as_deref()
                        .is_some_and(|o| o.contains(&format!("//{host}")))
            });
            if attempt.previous().len() > 5 {
                attempt.error("too many redirects")
            } else if allowed {
                attempt.follow()
            } else {
                attempt.error("a redirect away from the cached hosts")
            }
        });
        Ok(Cache {
            upstream: reqwest::Client::builder()
                .redirect(redirects)
                .user_agent(format!(
                    "releaseway/{}",
                    option_env!("RELEASEWAY_VERSION").unwrap_or("dev")
                ))
                .build()
                .map_err(|e| format!("an HTTP client: {e}"))?,
            http: reqwest::Client::new(),
            bucket,
            credentials: Credentials::new(
                config.bucket.access_key_id,
                config.bucket.secret_access_key,
            ),
            hosts: config.hosts,
            token: config.token.filter(|t| !t.is_empty()),
            scratch: config.scratch,
            most_bytes: config.most_bytes,
            downloads: AtomicU64::new(0),
            origin: config.origin,
        })
    }

    /// Answers `GET <path>`: from the bucket, or from upstream by way of it.
    pub async fn serve(&self, path: &str, query: Option<&str>, headers: &HeaderMap) -> Response {
        let Some(named) = named(path, &self.hosts).filter(|_| query.is_none()) else {
            return (StatusCode::NOT_FOUND, "not something this caches\n").into_response();
        };
        match self.in_bucket(&named.key).await {
            Ok(Some(found)) => return found,
            Ok(None) => {}
            Err(e) => return failed("reading the bucket", &e),
        }
        if !self.authorized(headers) {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Basic realm=\"releaseway\"")],
                "not cached yet, and only fetched with the token\n",
            )
                .into_response();
        }
        let url = match &self.origin {
            Some(origin) => format!("{origin}/{}/{}", named.host, named.rest),
            None => format!("https://{}/{}", named.host, named.rest),
        };
        match self.fetch(&url, &named.key).await {
            Ok(Fetched::Stored(bytes)) => {
                tracing::info!(key = %named.key, bytes, "cached");
            }
            Ok(Fetched::Answered(status)) => {
                return (status, "upstream did not have it\n").into_response();
            }
            Err(e) => return failed("caching", &e),
        }
        match self.in_bucket(&named.key).await {
            Ok(Some(found)) => found,
            Ok(None) => failed("caching", "the bucket did not keep it"),
            Err(e) => failed("reading the bucket", &e),
        }
    }

    /// Whether `headers` carry the token: as a bearer token, or as the
    /// password of Basic auth, which is how Bazel, curl and most other
    /// tools send what `~/.netrc` has for a host, whatever the login.
    fn authorized(&self, headers: &HeaderMap) -> bool {
        let Some(token) = &self.token else {
            return false;
        };
        let Some(given) = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
        else {
            return false;
        };
        let offered = if let Some(bearer) = given.strip_prefix("Bearer ") {
            bearer.to_string()
        } else if let Some(basic) = given.strip_prefix("Basic ") {
            let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(basic.trim()) else {
                return false;
            };
            let decoded = String::from_utf8_lossy(&decoded).into_owned();
            match decoded.split_once(':') {
                Some((_, password)) => password.to_string(),
                None => return false,
            }
        } else {
            return false;
        };
        same(offered.as_bytes(), token.as_bytes())
    }

    /// The object at `key`, as the answer to the request for it; None when
    /// the bucket has no such object.
    async fn in_bucket(&self, key: &str) -> Result<Option<Response>, String> {
        let url = self
            .bucket
            .get_object(Some(&self.credentials), key)
            .sign(SIGNED_FOR);
        let found = self.http.get(url).send().await.map_err(|e| e.to_string())?;
        if found.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !found.status().is_success() {
            return Err(format!("the bucket answered {}", found.status()));
        }
        let mut answer = Response::builder().status(StatusCode::OK);
        for name in [header::CONTENT_LENGTH, header::CONTENT_TYPE, header::ETAG] {
            if let Some(value) = found.headers().get(name.as_str())
                && let Ok(value) = HeaderValue::from_bytes(value.as_bytes())
            {
                answer = answer.header(name, value);
            }
        }
        answer
            .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
            .body(Body::from_stream(found.bytes_stream()))
            .map(Some)
            .map_err(|e| e.to_string())
    }

    /// Downloads `url` and puts it in the bucket as `key`, by way of a
    /// scratch file, so neither a large file nor a slow bucket is held in
    /// memory.
    async fn fetch(&self, url: &str, key: &str) -> Result<Fetched, String> {
        let mut download = self
            .upstream
            .get(url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !download.status().is_success() {
            // Upstream's own 404 is a 404 here; anything else is ours to say.
            let status = if download.status() == reqwest::StatusCode::NOT_FOUND {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_GATEWAY
            };
            return Ok(Fetched::Answered(status));
        }
        let most = self.most_bytes;
        if download.content_length().is_some_and(|n| n > most) {
            return Err(format!("{url} is more than {most} bytes"));
        }
        tokio::fs::create_dir_all(&self.scratch)
            .await
            .map_err(|e| format!("making {}: {e}", self.scratch.display()))?;
        let n = self.downloads.fetch_add(1, Ordering::Relaxed);
        let scratch = self.scratch.join(format!("download-{n}"));
        let result = async {
            let mut file = tokio::fs::File::create(&scratch)
                .await
                .map_err(|e| format!("making {}: {e}", scratch.display()))?;
            let mut bytes = 0u64;
            while let Some(chunk) = download.chunk().await.map_err(|e| e.to_string())? {
                bytes += chunk.len() as u64;
                if bytes > most {
                    return Err(format!("{url} is more than {most} bytes"));
                }
                file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            }
            file.flush().await.map_err(|e| e.to_string())?;
            drop(file);
            let put = self
                .bucket
                .put_object(Some(&self.credentials), key)
                .sign(SIGNED_FOR);
            let body = tokio::fs::File::open(&scratch)
                .await
                .map_err(|e| e.to_string())?;
            let stored = self
                .http
                .put(put)
                .header(reqwest::header::CONTENT_LENGTH, bytes)
                .body(body)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !stored.status().is_success() {
                return Err(format!(
                    "the bucket answered {} to the upload",
                    stored.status()
                ));
            }
            Ok(Fetched::Stored(bytes))
        }
        .await;
        let _ = tokio::fs::remove_file(&scratch).await;
        result
    }
}

enum Fetched {
    /// In the bucket now, this many bytes of it.
    Stored(u64),
    /// Upstream had nothing to give, and this is what to say instead.
    Answered(StatusCode),
}

/// What a request path names: a cached host and a path on it, and the
/// object it is kept as in the bucket, which is the two together.
#[derive(Debug, PartialEq)]
pub struct Named {
    pub host: String,
    pub rest: String,
    pub key: String,
}

/// `/<host>/<path>`, when the host is one of `hosts` and the path could
/// only ever name a file there: no empty, `.` or `..` segments, and nothing
/// but what a URL path has unencoded.
pub fn named(path: &str, hosts: &[String]) -> Option<Named> {
    let (host, rest) = path.strip_prefix('/')?.split_once('/')?;
    if !hosts.iter().any(|h| h == host) || rest.is_empty() {
        return None;
    }
    let tidy = rest
        .split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    let plain = rest
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-._~/%+@=,".contains(&b));
    (tidy && plain).then(|| Named {
        host: host.to_string(),
        rest: rest.to_string(),
        key: format!("{host}/{rest}"),
    })
}

/// Whether `a` and `b` are equal, taking as long whichever byte differs.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn failed(doing: &str, error: &str) -> Response {
    tracing::error!(error = %error, "{doing}");
    (StatusCode::BAD_GATEWAY, format!("{doing}: {error}\n")).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn github() -> Vec<String> {
        GITHUB.iter().map(|h| h.to_string()).collect()
    }

    #[test]
    fn only_the_configured_hosts_are_cached() {
        assert_eq!(
            named(
                "/github.com/bats-core/bats-core/archive/v1.10.0.tar.gz",
                &github()
            ),
            Some(Named {
                host: "github.com".into(),
                rest: "bats-core/bats-core/archive/v1.10.0.tar.gz".into(),
                key: "github.com/bats-core/bats-core/archive/v1.10.0.tar.gz".into(),
            })
        );
        assert!(
            named(
                "/github.com/k3s-io/k3s/releases/download/v1.33.4%2Bk3s1/k3s",
                &github()
            )
            .is_some()
        );
        for path in [
            "/example.com/a/b",
            "/github.com/",
            "/github.com",
            "/github.com/a/../b",
            "/github.com/a//b",
            "/github.com/a/b?x",
            "/github.com/a/b c",
            "github.com/a/b",
        ] {
            assert_eq!(named(path, &github()), None, "{path}");
        }
        assert!(named("/example.com/a/b", &["example.com".to_string()]).is_some());
    }

    #[test]
    fn the_token_is_compared_whole() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
    }
}
