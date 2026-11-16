use crate::server::{ApiResult, Shared};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::borrow::Cow;
#[derive(rust_embed::RustEmbed)]
#[folder = "assets/web/"]
struct Assets;
fn file(path: &str) -> Response {
    if path.split('/').any(|s| s == ".." || s == ".") || path.contains('\\') {
        return StatusCode::NOT_FOUND.into_response();
    }
    match Assets::get(path) {
        Some(file) => {
            let mime = if path.ends_with(".html") {
                "text/html; charset=utf-8"
            } else if path.ends_with(".js") || path.ends_with(".mjs") {
                "text/javascript; charset=utf-8"
            } else if path.ends_with(".wasm") {
                "application/wasm"
            } else if path.ends_with(".css") {
                "text/css"
            } else if path.ends_with(".json") {
                "application/json"
            } else {
                "application/octet-stream"
            };
            (
                [
                    ("Content-Type", mime),
                    ("Cache-Control", "no-cache"),
                    ("X-Content-Type-Options", "nosniff"),
                ],
                match file.data {
                    Cow::Borrowed(data) => bytes::Bytes::from_static(data),
                    Cow::Owned(data) => bytes::Bytes::from(data),
                },
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
async fn overview(State(a): State<Shared>) -> Json<Value> {
    let channels = a
        .device
        .lock()
        .await
        .items
        .iter()
        .filter_map(|i| {
            i.service
                .as_ref()
                .map(|s| json!({"id":s["id"],"name":s["name"],"title":i.title}))
        })
        .collect::<Vec<_>>();
    let mut jobs = a.recorder.snapshot();
    jobs.sort_by(|a, b| {
        b["created_at"]
            .as_f64()
            .unwrap_or(0.)
            .total_cmp(&a["created_at"].as_f64().unwrap_or(0.))
    });
    Json(
        json!({"enabled":a.recorder.enabled,"jobs":jobs,"channels":channels,"programs":a.programs(),"receive_slots":a.config.max_streams}),
    )
}
async fn recorded(State(a): State<Shared>) -> ApiResult<Json<Value>> {
    let items = a
        .recordings()
        .await
        .map_err(|_| (StatusCode::BAD_GATEWAY, "Catalogue unavailable"))?;
    Ok(Json(json!(items.into_iter().map(|i|json!({"id":i.token,"title":i.title,"channel":i.channel,"date":i.date,"duration":i.duration})).collect::<Vec<_>>())))
}
async fn create(
    State(a): State<Shared>,
    Json(body): Json<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    Ok((
        StatusCode::CREATED,
        Json(a.recorder.create(&a, body).await.map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "Invalid recording request or configuration",
            )
        })?),
    ))
}
async fn stop(State(a): State<Shared>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(
        a.recorder
            .stop(&id)
            .map_err(|_| (StatusCode::NOT_FOUND, "Unknown job"))?,
    ))
}
async fn download(
    State(a): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let job = a
        .recorder
        .get(&id)
        .filter(|j| matches!(j["status"].as_str(), Some("completed" | "stopped")))
        .ok_or((StatusCode::NOT_FOUND, "Recording unavailable"))?;
    let path = a
        .recorder
        .path(&job)
        .map_err(|_| (StatusCode::NOT_FOUND, "Invalid recording path"))?;
    let mut f = tokio::fs::File::open(&path)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "Recording unavailable"))?;
    let size = f
        .metadata()
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "Recording unavailable"))?
        .len();
    let mut begin = 0;
    let mut end = size.saturating_sub(1);
    let mut status = StatusCode::OK;
    if let Some(raw) = headers.get("range") {
        let raw = raw
            .to_str()
            .ok()
            .and_then(|v| v.strip_prefix("bytes="))
            .filter(|v| !v.contains(','))
            .ok_or((StatusCode::RANGE_NOT_SATISFIABLE, "Invalid range"))?;
        let (left, right) = raw
            .split_once('-')
            .ok_or((StatusCode::RANGE_NOT_SATISFIABLE, "Invalid range"))?;
        if left.is_empty() {
            let suffix = right
                .parse::<u64>()
                .map_err(|_| (StatusCode::RANGE_NOT_SATISFIABLE, "Invalid range"))?;
            if suffix == 0 {
                return Err((StatusCode::RANGE_NOT_SATISFIABLE, "Empty range"));
            }
            begin = size.saturating_sub(suffix);
        } else {
            begin = left
                .parse()
                .map_err(|_| (StatusCode::RANGE_NOT_SATISFIABLE, "Invalid range"))?;
            if !right.is_empty() {
                end = right
                    .parse::<u64>()
                    .map_err(|_| (StatusCode::RANGE_NOT_SATISFIABLE, "Invalid range"))?
                    .min(end);
            }
        }
        if begin >= size || begin > end {
            return Err((StatusCode::RANGE_NOT_SATISFIABLE, "Range exceeds file"));
        }
        status = StatusCode::PARTIAL_CONTENT;
    }
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    f.seek(std::io::SeekFrom::Start(begin))
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Seek failed"))?;
    let mut response =
        Body::from_stream(tokio_util::io::ReaderStream::new(f.take(end - begin + 1)))
            .into_response();
    *response.status_mut() = status;
    let h = response.headers_mut();
    h.insert("Content-Type", "video/MP2T".parse().unwrap());
    h.insert("Accept-Ranges", "bytes".parse().unwrap());
    h.insert("Cache-Control", "no-store".parse().unwrap());
    h.insert(
        "Content-Length",
        (end - begin + 1).to_string().parse().unwrap(),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        h.insert(
            "Content-Range",
            format!("bytes {begin}-{end}/{size}").parse().unwrap(),
        );
    }
    Ok(response)
}
pub(crate) fn routes() -> Router<Shared> {
    Router::new()
        .route(
            "/xa401/recordings",
            get(|| async { file("recordings.html") }),
        )
        .route(
            "/api/xa401/recordings",
            get(|| async { file("recordings.html") }),
        )
        .route(
            "/api/xa401/original",
            get(|| async { file("original.html") }),
        )
        .route(
            "/api/xa401/desktop-player",
            get(|| async { file("desktop-player.html") }),
        )
        .route(
            "/xa401/recordings.js",
            get(|| async { file("recordings.js") }),
        )
        .route(
            "/xa401/timeline.mjs",
            get(|| async { file("timeline.mjs") }),
        )
        .route(
            "/xa401/playback-session.mjs",
            get(|| async { file("playback-session.mjs") }),
        )
        .route(
            "/xa401/native-player.mjs",
            get(|| async { file("native-player.mjs") }),
        )
        .route(
            "/xa401/desktop-player.mjs",
            get(|| async { file("desktop-player.mjs") }),
        )
        .route(
            "/xa401/player-lib/{*path}",
            get(|Path(path): Path<String>| async move { file(&format!("player-lib/{path}")) }),
        )
        .route("/xa401/api/recording", get(overview))
        .route("/xa401/api/recorded", get(recorded))
        .route("/xa401/api/jobs", post(create))
        .route("/xa401/api/jobs/{id}/stop", post(stop))
        .route("/xa401/api/jobs/{id}/file", get(download))
        .layer(axum::extract::DefaultBodyLimit::max(4096))
}
