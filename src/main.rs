use std::env::VarError;
use std::num::NonZeroU16;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{http::header::HeaderMap, routing::get, Router};
use futures::future::{BoxFuture, FutureExt};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use ttl_cache::TtlCache;

#[tokio::main]
async fn main() {
    let port = std::env::var_os("PORT").map_or_else(
        || "3000".to_owned(),
        |s| s.into_string().expect("Failed to parse $PORT"),
    );

    let default_auth_header = match std::env::var("DEFAULT_AUTH_HEADER") {
        Ok(value) => {
            let result = value.parse();
            match result {
                Ok(value) => Some(value),
                Err(err) => panic!("Failed to parse default auth header as header: {:?}", err),
            }
        }
        Err(VarError::NotPresent) => None,
        Err(VarError::NotUnicode(_)) => panic!("Failed to parse default auth header as unicode"),
    };

    let app = Router::new()
        .route("/*path", get(handler))
        .route("/cached/:minutes/*path", get(cached_handler))
        .with_state(AppState {
            client: reqwest::Client::new(),
            cache: Arc::new(Mutex::new(TtlCache::new(10000))),
            default_auth_header,
        });

    axum::Server::bind(
        &format!("0.0.0.0:{port}")
            .parse()
            .expect("Failed to parse SocketAddr"),
    )
    .serve(app.into_make_service())
    .await
    .unwrap();
}

async fn cached_handler(
    State(state): State<AppState>,
    Path((minutes, path)): Path<(NonZeroU16, String)>,
    mut headers: HeaderMap,
) -> impl IntoResponse {
    if !headers.contains_key(axum::http::header::AUTHORIZATION) {
        if let Some(default_auth_header) = state.default_auth_header {
            headers.append(axum::http::header::AUTHORIZATION, default_auth_header);
        }
    };
    let key = CacheKey {
        authorization_header: headers
            .get(axum::http::header::AUTHORIZATION)
            .map(|h| h.as_bytes().to_owned()),
        path: path.clone(),
    };
    let max_duration = Duration::from_secs(u64::from(u16::from(minutes) * 60));
    {
        let cache = state.cache.lock().unwrap();
        if let Some(value) = cache.get(&key) {
            if Instant::now().duration_since(value.generated_at) <= max_duration {
                return serialize_for_response(&value.values);
            }
        }
    }
    match fetch_from_github(
        state.client,
        format!("https://api.github.com/{}", path),
        headers,
    )
    .await
    {
        Ok(github_response) => {
            let response = serialize_for_response(&github_response);
            if response.0.is_success() {
                let mut cache = state.cache.lock().unwrap();
                cache.insert(
                    key,
                    CacheValue {
                        generated_at: Instant::now(),
                        values: github_response,
                    },
                    max_duration,
                );
            }
            response
        }
        Err((status_code, err)) => (status_code, cors_allow_all(), err),
    }
}

async fn handler(
    State(state): State<AppState>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    match fetch_from_github(
        state.client,
        format!("https://api.github.com/{}", path),
        headers,
    )
    .await
    {
        Ok(response) => serialize_for_response(&response),
        Err((status_code, err)) => (status_code, cors_allow_all(), err),
    }
}

fn serialize_for_response(response: &OpaqueJsonArray) -> (StatusCode, HeaderMap, String) {
    match serde_json::to_string(response) {
        Ok(response) => (StatusCode::OK, cors_allow_all(), response),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            cors_allow_all(),
            format!("Failed to serialize response: {}", err),
        ),
    }
}

fn fetch_from_github(
    client: reqwest::Client,
    url: String,
    request_headers: HeaderMap,
) -> BoxFuture<'static, Result<OpaqueJsonArray, (StatusCode, String)>> {
    async move {
        let mut builder = client.get(&url);
        for (key, value) in request_headers.iter() {
            match key.as_str() {
                "host" => match Url::parse(&url) {
                    Ok(url) => {
                        if let Some(host) = url.host_str() {
                            builder = builder.header(key.clone(), host);
                        }
                    }
                    Err(err) => {
                        eprintln!(
                            "Skipping setting host header - Failed to parse URL from \"{}\": {}",
                            url, err
                        );
                    }
                },
                "accept-encoding" => {
                    // We don't handle decompression, so drop any requests for compression.
                }
                key => {
                    builder = builder.header(key, value.clone());
                }
            }
        }
        let response = builder.send().await.map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to make request to github: {:?}", err),
            )
        })?;
        if !response.status().is_success() {
            return Err((
                StatusCode::from_u16(response.status().as_u16())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                response
                    .text()
                    .await
                    .unwrap_or_else(|err| format!("Failed to read response body: {}", err)),
            ));
        }
        let mut response_headers = response.headers().clone();
        let response_body = response.text().await.map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read response: {}", err),
            )
        })?;
        let mut values: OpaqueJsonArray = serde_json::from_str(&response_body).map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read response \"{}\": {}", response_body, err),
            )
        })?;
        if let Some(link) = response_headers.remove("link") {
            let link_map = match link.to_str() {
                Ok(link) => match parse_link_header::parse(link) {
                    Ok(link_map) => link_map,
                    Err(err) => {
                        return Err((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("Failed to parse link map \"{}\": {}", link, err),
                        ))
                    }
                },
                Err(err) => {
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to parse link header \"{:?}\": {}", link, err),
                    ));
                }
            };
            if let Some(link) = link_map.get(&Some("next".to_owned())) {
                let rest = fetch_from_github(client, link.uri.to_string(), request_headers)
                    .await
                    .map_err(|err| {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("Failed to make follow-up request to github: {:?}", err),
                        )
                    })?;
                values.values.extend(rest.values);
            }
        }
        Ok(values)
    }
    .boxed()
}

fn cors_allow_all() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        "*".parse().unwrap(),
    );
    headers
}

#[derive(Deserialize, Serialize)]
#[serde(transparent)]
struct OpaqueJsonArray {
    #[serde(flatten)]
    values: Vec<serde_json::Value>,
}

#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    cache: Arc<Mutex<TtlCache<CacheKey, CacheValue>>>,
    default_auth_header: Option<axum::http::header::HeaderValue>,
}

#[derive(Hash, PartialEq, Eq)]
struct CacheKey {
    authorization_header: Option<Vec<u8>>,
    path: String,
}

struct CacheValue {
    values: OpaqueJsonArray,
    generated_at: std::time::Instant,
}
