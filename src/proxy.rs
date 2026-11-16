//! Fixed loopback Konomi upstream. This is never a caller-selectable open proxy.
use anyhow::{Result, ensure};
use axum::{
    body::{Body, to_bytes},
    extract::{
        FromRequestParts, Request,
        ws::{Message, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use std::{sync::OnceLock, time::Duration};
pub fn client() -> Result<reqwest::Client> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(40))
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(4)
        .build()?;
    let _ = CLIENT.set(client);
    Ok(CLIENT.get().expect("Client just initialized").clone())
}
pub async fn frontend(request: Request, next: axum::middleware::Next) -> Response {
    let path = request.uri().path();
    let own = path.starts_with("/xa401/")
        || path.starts_with("/api/xa401/")
        || path == "/api/videos"
        || path.starts_with("/api/videos/")
        || path.starts_with("/api/recording/");
    if request
        .headers()
        .get("x-xa401-frontend")
        .is_some_and(|v| v == "1")
        && !own
    {
        return forward(request).await;
    }
    next.run(request).await
}
pub async fn json_response(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<serde_json::Value> {
    ensure!(response.status().is_success(), "Local upstream failed");
    let mut data = vec![];
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            data.len() + chunk.len() <= limit,
            "Local JSON exceeds limit"
        );
        data.extend(chunk);
    }
    Ok(serde_json::from_slice(&data)?)
}
fn clean(headers: &HeaderMap) -> HeaderMap {
    let tokens = headers
        .get("connection")
        .and_then(|s| s.to_str().ok())
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .collect::<Vec<_>>();
    headers
        .iter()
        .filter(|(k, _)| {
            ![
                "connection",
                "keep-alive",
                "proxy-authenticate",
                "proxy-authorization",
                "te",
                "trailer",
                "transfer-encoding",
                "upgrade",
                "host",
            ]
            .contains(&k.as_str())
                && !tokens.iter().any(|s| s == k.as_str())
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}
pub async fn forward(request: Request) -> Response {
    if request
        .headers()
        .get("upgrade")
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
    {
        return websocket(request).await;
    }
    let result = async {
        let (parts, body) = request.into_parts();
        let target = format!(
            "http://127.0.0.77:7010{}",
            parts
                .uri
                .path_and_query()
                .map(|s| s.as_str())
                .unwrap_or("/")
        );
        let headers = clean(&parts.headers);
        let body = tokio::time::timeout(Duration::from_secs(30), to_bytes(body, 40 * 1024 * 1024))
            .await??;
        let upstream = client()?
            .request(parts.method, target)
            .headers(headers)
            .body(body)
            .send()
            .await?;
        let status = upstream.status();
        let headers = clean(upstream.headers());
        let body = Body::from_stream(upstream.bytes_stream());
        let mut response = body.into_response();
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        Ok::<_, anyhow::Error>(response)
    }
    .await;
    result
        .unwrap_or_else(|_| (StatusCode::BAD_GATEWAY, "Local KonomiTV unavailable").into_response())
}
async fn websocket(request: Request) -> Response {
    let (mut parts, _) = request.into_parts();
    let Ok(upgrade) = WebSocketUpgrade::from_request_parts(&mut parts, &()).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    use tokio_tungstenite::tungstenite::{Message as RemoteMessage, client::IntoClientRequest};
    let target = format!(
        "ws://127.0.0.77:7010{}",
        parts
            .uri
            .path_and_query()
            .map(|s| s.as_str())
            .unwrap_or("/")
    );
    let Ok(mut remote_request) = target.into_client_request() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    for (k, v) in clean(&parts.headers) {
        if let Some(k) = k {
            if !k.as_str().starts_with("sec-websocket-") {
                remote_request.headers_mut().insert(k, v);
            }
        }
    }
    if let Some(protocol) = parts.headers.get("sec-websocket-protocol") {
        remote_request
            .headers_mut()
            .insert("sec-websocket-protocol", protocol.clone());
    }
    let remote = tokio::time::timeout(
        Duration::from_secs(8),
        tokio_tungstenite::connect_async(remote_request),
    )
    .await;
    let Ok(Ok((remote, reply))) = remote else {
        return (StatusCode::BAD_GATEWAY, "Konomi websocket unavailable").into_response();
    };
    let selected = reply
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let upgrade = if let Some(p) = selected {
        upgrade.protocols([p])
    } else {
        upgrade
    };
    upgrade
        .max_message_size(40 * 1024 * 1024)
        .on_upgrade(move |local| async move {
            let (mut lw, mut lr) = local.split();
            let (mut rw, mut rr) = remote.split();
            let to_remote = async {
                while let Some(Ok(m)) = lr.next().await {
                    let m = match m {
                        Message::Text(t) => RemoteMessage::Text(t.to_string().into()),
                        Message::Binary(b) => RemoteMessage::Binary(b),
                        Message::Ping(b) => RemoteMessage::Ping(b),
                        Message::Pong(b) => RemoteMessage::Pong(b),
                        Message::Close(_) => break,
                    };
                    if rw.send(m).await.is_err() {
                        break;
                    }
                }
            };
            let to_local = async {
                while let Some(Ok(m)) = rr.next().await {
                    let m = match m {
                        RemoteMessage::Text(t) => Message::Text(t.to_string().into()),
                        RemoteMessage::Binary(b) => Message::Binary(b),
                        RemoteMessage::Ping(b) => Message::Ping(b),
                        RemoteMessage::Pong(b) => Message::Pong(b),
                        RemoteMessage::Close(_) => break,
                        _ => continue,
                    };
                    if lw.send(m).await.is_err() {
                        break;
                    }
                }
            };
            tokio::select! {_=to_remote=>(),_=to_local=>()}
        })
        .into_response()
}
