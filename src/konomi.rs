//! Read-only XA401 recordings merged into KonomiTV's existing schemas/UI.
use crate::{device::Item, server::Shared};
use anyhow::{Result, ensure};
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
pub const REMOTE_MIN: u64 = 1 << 48;
pub fn video_id(udn: &str, token: &str) -> u64 {
    REMOTE_MIN
        + u64::from_str_radix(
            &hex::encode(Sha256::digest(format!("{udn}:{token}").as_bytes()))[..12],
            16,
        )
        .unwrap()
}
fn seconds(value: &str) -> Result<f64> {
    let p = value.split(':').collect::<Vec<_>>();
    ensure!(p.len() == 3, "Invalid duration");
    let h = p[0].parse::<u32>()?;
    let m = p[1].parse::<u32>()?;
    let s = p[2].parse::<f64>()?;
    let total = h as f64 * 3600. + m as f64 * 60. + s;
    ensure!(
        m < 60 && s.is_finite() && (0. ..60.).contains(&s) && total > 0. && total <= 86400.,
        "Invalid duration"
    );
    Ok(total)
}
fn row(udn: &str, item: &Item) -> Result<Value> {
    let resource = item
        .resources
        .iter()
        .find(|r| !r.url.contains("quality="))
        .ok_or_else(|| anyhow::anyhow!("Original missing"))?;
    let duration = seconds(
        resource
            .duration
            .as_deref()
            .or(item.duration.as_deref())
            .ok_or_else(|| anyhow::anyhow!("No duration"))?,
    )?;
    let start = chrono::DateTime::parse_from_rfc3339(&item.date).or_else(|_| {
        chrono::NaiveDateTime::parse_from_str(&item.date, "%Y-%m-%dT%H:%M:%S").map(|n| {
            n.and_local_timezone(chrono::FixedOffset::east_opt(9 * 3600).unwrap())
                .unwrap()
        })
    })?;
    let begin = start.to_rfc3339();
    let end =
        (start + chrono::Duration::milliseconds((duration * 1000.).round() as i64)).to_rfc3339();
    let id = video_id(udn, &item.token);
    let mut p: Value = serde_json::from_str(include_str!("../templates/konomi-video.json"))?;
    p["id"] = json!(id);
    p["title"] = json!(item.title);
    p["description"] = json!(if item.description.is_empty() {
        "番組概要を取得できませんでした。"
    } else {
        &item.description
    });
    p["detail"]["保存元"] = json!(format!(
        "XA401（{}）から直接再生",
        if item.channel.is_empty() {
            "放送局未取得"
        } else {
            &item.channel
        }
    ));
    p["start_time"] = json!(begin);
    p["end_time"] = json!(end);
    p["duration"] = json!(duration);
    p["created_at"] = json!(begin);
    p["updated_at"] = json!(begin);
    p["external_playback"]["stream_url"] =
        json!(format!("/xa401/api/recorded/{}/stream", item.token));
    p["external_playback"]["duration"] = json!(duration);
    let profile = resource
        .protocol_info
        .split(';')
        .find_map(|s| s.split_once("DLNA.ORG_PN=").map(|(_, v)| v))
        .unwrap_or("");
    p["external_playback"]["original_available"] =
        json!(profile.trim_start_matches("DTCP_").starts_with("MPEG_TS_"));
    let v = &mut p["recorded_video"];
    v["id"] = json!(id);
    v["file_path"] = json!(format!("xa401://{}/{}", item.token, item.title));
    v["duration"] = json!(duration);
    for key in [
        "file_created_at",
        "file_modified_at",
        "recording_start_time",
        "created_at",
        "updated_at",
    ] {
        v[key] = json!(begin)
    }
    v["recording_end_time"] = json!(end);
    v["file_size"] = json!(
        resource
            .attributes
            .get("cleartextSize")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    );
    v["video_codec"] = json!(if profile.starts_with("MPEG_TS_") {
        "MPEG-2"
    } else {
        "未取得"
    });
    if let Some((w, h)) = resource
        .attributes
        .get("resolution")
        .and_then(|v| v.split_once('x'))
    {
        if let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) {
            v["video_resolution_width"] = json!(w);
            v["video_resolution_height"] = json!(h);
        }
    }
    Ok(p)
}
async fn remote(a: &Shared) -> Result<Vec<Value>> {
    let mut rows = vec![];
    let mut ids = std::collections::HashSet::new();
    for item in a.recordings().await? {
        if let Ok(p) = row(&a.config.udn, &item) {
            ensure!(
                ids.insert(p["id"].as_u64().unwrap()),
                "External identity collision"
            );
            rows.push(p);
        }
    }
    Ok(rows)
}
async fn native() -> Result<Vec<Value>> {
    let client = crate::proxy::client()?;
    let mut rows = vec![];
    for page in 1..=334 {
        let response = client
            .get(format!("http://127.0.0.77:7010/api/videos?page={page}"))
            .send()
            .await?;
        let value = crate::proxy::json_response(response, 8 * 1024 * 1024).await?;
        let batch = value["recorded_programs"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("No Konomi catalogue"))?;
        ensure!(
            batch
                .iter()
                .all(|p| p["id"].as_u64().is_some_and(|n| n < REMOTE_MIN)),
            "ID namespace collision"
        );
        rows.extend(batch.iter().cloned());
        if rows.len() >= value["total"].as_u64().unwrap_or(u64::MAX) as usize {
            return Ok(rows);
        }
        ensure!(!batch.is_empty(), "Pagination changed");
    }
    anyhow::bail!("Catalogue exceeds limit")
}
fn page(mut rows: Vec<Value>, query: Option<String>) -> Result<Value> {
    let pairs = url::form_urlencoded::parse(query.as_deref().unwrap_or("").as_bytes())
        .into_owned()
        .collect::<Vec<_>>();
    let get = |key: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    let page = get("page").unwrap_or("1").parse::<usize>()?;
    ensure!((1..=100000).contains(&page), "Invalid page");
    let order = get("order").unwrap_or("desc");
    ensure!(
        ["asc", "desc", "file_path_asc", "file_path_desc", "ids"].contains(&order),
        "Invalid order"
    );
    if order == "ids" {
        let ids = pairs
            .iter()
            .filter(|(k, _)| k == "ids")
            .map(|(_, v)| v.parse::<u64>())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(ids.len() <= 100, "Too many IDs");
        let lookup = rows
            .into_iter()
            .map(|p| (p["id"].as_u64().unwrap_or(0), p))
            .collect::<std::collections::HashMap<_, _>>();
        let mut seen = std::collections::HashSet::new();
        rows = ids
            .into_iter()
            .filter(|n| seen.insert(*n))
            .filter_map(|n| lookup.get(&n).cloned())
            .collect();
    } else {
        let query = get("query").unwrap_or("").to_lowercase();
        let terms = query.split_whitespace().collect::<Vec<_>>();
        rows.retain(|p| {
            let haystack = format!(
                "{}\n{}",
                p["title"].as_str().unwrap_or(""),
                p["description"].as_str().unwrap_or("")
            )
            .to_lowercase();
            terms.iter().all(|s| haystack.contains(s))
        });
        if order.starts_with("file_path_") {
            rows.sort_by(|a, b| {
                a["recorded_video"]["file_path"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["recorded_video"]["file_path"].as_str().unwrap_or(""))
            });
        } else {
            rows.sort_by(|a, b| {
                a["start_time"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["start_time"].as_str().unwrap_or(""))
            });
        }
        if order.ends_with("desc") {
            rows.reverse();
        }
    }
    let total = rows.len();
    Ok(
        json!({"total":total,"recorded_programs":rows.into_iter().skip((page-1)*30).take(30).collect::<Vec<_>>()}),
    )
}
async fn handler(State(a): State<Shared>, request: Request) -> Response {
    let path = request.uri().path().to_string();
    let prefix = if path.starts_with("/api/xa401/konomi/videos") {
        "/api/xa401/konomi/videos"
    } else {
        "/api/videos"
    };
    let tail = path
        .strip_prefix(prefix)
        .unwrap_or("")
        .trim_matches('/')
        .to_string();
    if let Some(id) = tail
        .split('/')
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n < REMOTE_MIN)
    {
        let _ = id;
        return crate::proxy::forward(request).await;
    }
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            "XA401 recordings are read-only",
        )
            .into_response();
    }
    let query = request.uri().query().map(str::to_string);
    drop(request);
    let result:Result<Response>=async{
        let mut rows=remote(&a).await?;
        if tail.is_empty()||tail=="search"{rows.extend(native().await?);return Ok(Json(page(rows,query.clone())?).into_response())}
        let mut parts=tail.split('/');let id=parts.next().unwrap_or("").parse::<u64>()?;let Some(mut p)=rows.into_iter().find(|p|p["id"].as_u64()==Some(id))else{return Ok(StatusCode::NOT_FOUND.into_response())};
        match parts.next(){None=>{let token=p["external_playback"]["stream_url"].as_str().unwrap().split('/').nth_back(1).unwrap();let source=crate::server::recorded_source_value(&a,token).await?;let duration=source["duration"].clone();p["duration"]=duration.clone();p["recorded_video"]["duration"]=duration.clone();p["external_playback"]["duration"]=duration.clone();let end=(chrono::DateTime::parse_from_rfc3339(p["start_time"].as_str().unwrap())?+chrono::Duration::milliseconds((duration.as_f64().unwrap()*1000.).round() as i64)).to_rfc3339();p["end_time"]=json!(end);p["recorded_video"]["recording_end_time"]=json!(end);Ok(Json(p).into_response())},
        Some("thumbnail"|"thumbnail-tile")=>Ok(([("Content-Type","image/svg+xml")],"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"480\" height=\"270\"><rect width=\"480\" height=\"270\" fill=\"#30221f\"/><text x=\"240\" y=\"135\" text-anchor=\"middle\" fill=\"#dbb\" font-size=\"28\">XA401</text></svg>").into_response()),
        Some("jikkyo")=>Ok(Json(json!({"is_success":false,"comments":[],"detail":"XA401録画の実況ログは未対応です。"})).into_response()),_=>Ok(StatusCode::NOT_IMPLEMENTED.into_response())}
    }.await;
    match result {
        Ok(response) => response,
        Err(_) => (StatusCode::BAD_GATEWAY, "Recording metadata unavailable").into_response(),
    }
}
pub(crate) fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/xa401/konomi/videos", any(handler))
        .route("/api/xa401/konomi/videos/{*tail}", any(handler))
        .route("/api/videos", any(handler))
        .route("/api/videos/", any(handler))
        .route("/api/videos/{*tail}", any(handler))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable_namespace() {
        assert!(video_id("uuid:example", "000000000000000000000000") >= REMOTE_MIN);
        assert_eq!(seconds("01:02:03.500").unwrap(), 3723.5);
        assert!(seconds("00:60:00").is_err());
    }
}
