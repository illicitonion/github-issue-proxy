use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{http::header::HeaderMap, routing::get, Router};
use futures::future::{BoxFuture, FutureExt};
use reqwest::Url;
use serde::{Deserialize, Serialize};

#[tokio::main]
async fn main() {
    let port = std::env::var_os("PORT").map_or_else(
        || "3000".to_owned(),
        |s| s.into_string().expect("Failed to parse $PORT"),
    );

    let app = Router::new().route("/*path", get(handler));

    axum::Server::bind(
        &format!("0.0.0.0:{port}")
            .parse()
            .expect("Failed to parse SocketAddr"),
    )
    .serve(app.into_make_service())
    .await
    .unwrap();
}

async fn handler(Path(path): Path<String>, headers: HeaderMap) -> impl IntoResponse {
    let client = reqwest::Client::new();
    match fetch_from_github(client, format!("https://api.github.com/{}", path), headers).await {
        Ok(response) => match serde_json::to_string(&response) {
            Ok(response) => (StatusCode::OK, response),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to serialize response: {}", err),
            ),
        },
        Err((status_code, err)) => (status_code, err),
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
            if key.as_str() == "host" {
                match Url::parse(&url) {
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
                }
            } else {
                builder = builder.header(key.clone(), value.clone());
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
        let mut values: OpaqueJsonArray = response.json().await.map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to parse response from github: {:?}", err),
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

#[derive(Deserialize, Serialize)]
#[serde(transparent)]
struct OpaqueJsonArray {
    #[serde(flatten)]
    values: Vec<serde_json::Value>,
}
