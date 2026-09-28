//! releaseway: a pull-through cache of release and archive downloads in an
//! S3 bucket (`cache.rs`). Configured from the environment; see README.md.

mod cache;

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::get;
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing::Level;

use cache::{BucketConfig, Cache, Config};

/// The cache, or None before it has a bucket, when every file is a 503.
fn router(cache: Option<Arc<Cache>>) -> Router {
    let file = get(
        |State(cache): State<Option<Arc<Cache>>>, uri: Uri, headers: HeaderMap| async move {
            match cache {
                // The path as it was asked for, still percent-encoded, which
                // is the name upstream has it under and the key it is kept as.
                Some(cache) => cache.serve(uri.path(), uri.query(), &headers).await,
                None => {
                    (StatusCode::SERVICE_UNAVAILABLE, "no bucket is configured\n").into_response()
                }
            }
        },
    );
    let logged =
        TraceLayer::new_for_http().on_response(DefaultOnResponse::new().level(Level::INFO));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .fallback(file.layer(logged))
        .with_state(cache)
}

/// `name`, when it is set to something.
fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The cache as the environment configures it, or why there is none.
fn configured() -> Result<Cache, String> {
    let need = |name: &str| env(name).ok_or_else(|| format!("{name} is not set"));
    let hosts = match env("RELEASEWAY_HOSTS") {
        Some(hosts) => hosts
            .split([',', ' ', '\n'])
            .filter(|h| !h.is_empty())
            .map(String::from)
            .collect(),
        None => cache::GITHUB.iter().map(|h| h.to_string()).collect(),
    };
    let most_bytes = match env("RELEASEWAY_MAX_BYTES") {
        Some(n) => n
            .parse()
            .map_err(|_| format!("RELEASEWAY_MAX_BYTES is not a number: {n}"))?,
        None => 2 << 30,
    };
    Cache::new(Config {
        bucket: BucketConfig {
            endpoint: need("RELEASEWAY_BUCKET_ENDPOINT")?,
            name: need("RELEASEWAY_BUCKET")?,
            region: env("RELEASEWAY_BUCKET_REGION").unwrap_or_else(|| "auto".to_string()),
            access_key_id: need("RELEASEWAY_ACCESS_KEY_ID")?,
            secret_access_key: need("RELEASEWAY_SECRET_ACCESS_KEY")?,
        },
        hosts,
        token: env("RELEASEWAY_TOKEN"),
        scratch: PathBuf::from(env("RELEASEWAY_SCRATCH").unwrap_or_else(|| "/tmp".to_string())),
        most_bytes,
        origin: None,
    })
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .json()
        .with_max_level(Level::INFO)
        .init();
    if env("RELEASEWAY_TOKEN").is_none() {
        tracing::warn!("RELEASEWAY_TOKEN is not set: only what is cached already is served");
    }
    // Up without a bucket rather than not at all, so a deploy before its
    // Secret is made still rolls out, and says why in its log.
    let cache = match configured() {
        Ok(cache) => Some(Arc::new(cache)),
        Err(why) => {
            tracing::error!("no cache: {why}; every file is a 503");
            None
        }
    };
    let listen = env("LISTEN_ADDR").unwrap_or_else(|| "0.0.0.0:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .unwrap_or_else(|e| panic!("listening on {listen}: {e}"));
    tracing::info!("listening on {listen}");
    axum::serve(listener, router(cache))
        .with_graceful_shutdown(stopped())
        .await
        .expect("serving");
}

#[cfg(unix)]
async fn stopped() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM");
    tokio::select! {
        _ = terminate.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[cfg(not(unix))]
async fn stopped() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::body::{Body, Bytes};
    use axum::http::{Request, header};
    use axum::response::Redirect;
    use axum::routing::put;
    use base64::Engine;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    const TOKEN: &str = "sekrit";

    /// A bucket that keeps what is put in it by path and ignores the
    /// signature, which is the real bucket's to check.
    #[derive(Default)]
    struct StandIn {
        objects: Mutex<HashMap<String, Bytes>>,
        /// How many times upstream was asked for anything.
        asked: AtomicUsize,
    }

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{address}")
    }

    /// The bucket and GitHub, each on a port of its own, and the cache in
    /// front of them.
    async fn cache() -> (Router, Arc<StandIn>) {
        let stand_in = Arc::new(StandIn::default());
        let bucket = Router::new()
            .route(
                "/{*key}",
                put(
                    |State(s): State<Arc<StandIn>>, uri: Uri, body: Bytes| async move {
                        s.objects
                            .lock()
                            .unwrap()
                            .insert(uri.path().to_string(), body);
                        StatusCode::OK
                    },
                )
                .get(|State(s): State<Arc<StandIn>>, uri: Uri| async move {
                    match s.objects.lock().unwrap().get(uri.path()) {
                        Some(body) => body.clone().into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                }),
            )
            .with_state(stand_in.clone());
        let bucket = serve(bucket).await;
        let github = Router::new()
            .route(
                "/github.com/owner/repo/archive/v1.tar.gz",
                get(|State(s): State<Arc<StandIn>>| async move {
                    s.asked.fetch_add(1, Ordering::Relaxed);
                    // As GitHub does, to codeload.
                    Redirect::temporary("/codeload.github.com/owner/repo/tar.gz/refs/tags/v1")
                }),
            )
            .route(
                "/codeload.github.com/owner/repo/tar.gz/refs/tags/v1",
                get(|| async { "the archive" }),
            )
            .fallback(|State(s): State<Arc<StandIn>>| async move {
                s.asked.fetch_add(1, Ordering::Relaxed);
                StatusCode::NOT_FOUND
            })
            .with_state(stand_in.clone());
        let github = serve(github).await;
        let scratch = std::env::temp_dir().join(format!("releaseway-test-{}", std::process::id()));
        let cache = Cache::new(Config {
            bucket: BucketConfig {
                endpoint: bucket,
                name: "cache".into(),
                region: "auto".into(),
                access_key_id: "id".into(),
                secret_access_key: "secret".into(),
            },
            hosts: cache::GITHUB.iter().map(|h| h.to_string()).collect(),
            token: Some(TOKEN.into()),
            scratch,
            most_bytes: 1 << 20,
            origin: Some(github),
        })
        .unwrap();
        (router(Some(Arc::new(cache))), stand_in)
    }

    async fn get_with(router: &Router, path: &str, auth: Option<String>) -> (StatusCode, String) {
        let mut request = Request::get(path);
        if let Some(auth) = auth {
            request = request.header(header::AUTHORIZATION, auth);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    fn netrc(password: &str) -> Option<String> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!("bazel:{password}"));
        Some(format!("Basic {encoded}"))
    }

    const ARCHIVE: &str = "/github.com/owner/repo/archive/v1.tar.gz";

    #[tokio::test]
    async fn a_download_is_fetched_once_with_the_token_and_then_served_to_anyone() {
        let (router, stand_in) = cache().await;

        // Not cached yet, and no token: nothing is fetched.
        assert_eq!(
            get_with(&router, ARCHIVE, None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get_with(&router, ARCHIVE, netrc("wrong")).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(stand_in.asked.load(Ordering::Relaxed), 0);

        // With it, as ~/.netrc sends it: fetched, through GitHub's
        // redirect, and kept.
        assert_eq!(
            get_with(&router, ARCHIVE, netrc(TOKEN)).await,
            (StatusCode::OK, "the archive".to_string())
        );
        assert_eq!(stand_in.asked.load(Ordering::Relaxed), 1);
        assert_eq!(
            stand_in.objects.lock().unwrap().keys().collect::<Vec<_>>(),
            ["/cache/github.com/owner/repo/archive/v1.tar.gz"]
        );

        // Then anyone gets it, and upstream is not asked again.
        assert_eq!(
            get_with(&router, ARCHIVE, None).await,
            (StatusCode::OK, "the archive".to_string())
        );
        assert_eq!(stand_in.asked.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn what_upstream_does_not_have_is_not_kept() {
        let (router, stand_in) = cache().await;
        let missing = "/github.com/owner/repo/archive/v2.tar.gz";
        assert_eq!(
            get_with(&router, missing, Some(format!("Bearer {TOKEN}")))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert!(stand_in.objects.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn without_a_bucket_it_is_up_and_says_so() {
        let router = router(None);
        assert_eq!(get_with(&router, "/healthz", None).await.0, StatusCode::OK);
        assert_eq!(
            get_with(&router, ARCHIVE, None).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn only_the_configured_hosts_are_cached() {
        let (router, stand_in) = cache().await;
        for path in [
            "/example.com/a/b",
            "/github.com/a/../b",
            "/github.com/a/b?c=d",
        ] {
            assert_eq!(
                get_with(&router, path, Some(format!("Bearer {TOKEN}")))
                    .await
                    .0,
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }
        assert_eq!(stand_in.asked.load(Ordering::Relaxed), 0);
        assert_eq!(
            get_with(&router, "/healthz", None).await,
            (StatusCode::OK, "ok".to_string())
        );
    }
}
