//! Translate PC reservations only; retains the previous Python API contract.
use crate::server::Shared;
use anyhow::{Result, ensure};
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::{Value, json};
fn active(job: &Value) -> bool {
    job["source"] == "live"
        && matches!(
            job["status"].as_str(),
            Some("scheduled" | "waiting" | "recording" | "stopping" | "finalizing" | "disabled")
        )
}
fn id(job: &Value) -> Result<u64> {
    let s = job["id"]
        .as_str()
        .filter(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| anyhow::anyhow!("Invalid job identity"))?;
    Ok(u64::from_str_radix(&s[..12], 16)?)
}
fn iso(seconds: f64) -> Result<String> {
    Ok(
        chrono::DateTime::from_timestamp_millis((seconds * 1000.).round() as i64)
            .ok_or_else(|| anyhow::anyhow!("Invalid timestamp"))?
            .to_rfc3339(),
    )
}
fn row(
    job: &Value,
    service: &Value,
    channel: Option<Value>,
    jobs: &[Value],
    maximum: usize,
) -> Result<Value> {
    let mut p: Value = serde_json::from_str(include_str!("../templates/konomi-reservation.json"))?;
    let nid = service["networkId"].as_u64().unwrap_or(0);
    let sid = service["serviceId"].as_u64().unwrap_or(0);
    let channel_id = format!("NID{nid}-SID{sid:03}");
    let mut c = if let Some(c) = channel {
        c
    } else {
        let mut c = p["channel"].clone();
        c["id"] = json!(channel_id);
        c["display_channel_id"] = json!(channel_id);
        c["network_id"] = json!(nid);
        c["service_id"] = json!(sid);
        c["transport_stream_id"] = service["transportStreamId"].clone();
        c["remocon_id"] = service["remoteControlKeyId"].clone();
        c["type"] = service["channel"]["type"].clone();
        c["name"] = service["name"].clone();
        c
    };
    if let Some(c) = c.as_object_mut() {
        c.remove("program_present");
        c.remove("program_following");
    }
    let channel_id = c["id"].as_str().unwrap_or(&channel_id).to_string();
    p["channel"] = c;
    let start = job["start_at"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("Missing start"))?;
    let duration = job["duration_seconds"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("Missing duration"))?;
    let end = start + duration;
    p["id"] = json!(id(job)?);
    p["program"]["id"] = json!(format!("{channel_id}-EID65535"));
    p["program"]["channel_id"] = json!(channel_id);
    p["program"]["network_id"] = json!(nid);
    p["program"]["service_id"] = json!(sid);
    p["program"]["title"] = job["title"].clone();
    p["program"]["start_time"] = json!(iso(start)?);
    p["program"]["end_time"] = json!(iso(end)?);
    p["program"]["duration"] = json!(duration);
    p["is_recording_in_progress"] = json!(matches!(
        job["status"].as_str(),
        Some("recording" | "stopping" | "finalizing")
    ));
    p["scheduled_recording_file_name"] = job["file_name"].clone();
    p["record_settings"]["is_enabled"] = json!(job["status"] != "disabled");
    let mut events = vec![];
    for other in jobs.iter().filter(|j| j["status"] != "disabled") {
        let left = start.max(other["start_at"].as_f64().unwrap_or(end));
        let right = end.min(
            other["start_at"].as_f64().unwrap_or(0.)
                + other["duration_seconds"].as_f64().unwrap_or(0.),
        );
        if left < right {
            events.push((left, 1, other["source_key"].to_string()));
            events.push((right, -1, other["source_key"].to_string()));
        }
    }
    events.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut counts = std::collections::HashMap::new();
    let mut peak = 0;
    for (_, delta, key) in events {
        let n = counts.entry(key.clone()).or_insert(0);
        *n += delta;
        if *n == 0 {
            counts.remove(&key);
        }
        peak = peak.max(counts.len());
    }
    p["recording_availability"] = json!(if peak > maximum || job["status"] == "waiting" {
        "Partial"
    } else {
        "Full"
    });
    Ok(p)
}
async fn handler(State(a): State<Shared>, request: Request) -> Response {
    let path = request.uri().path().to_string();
    let method = request.method().clone();
    drop(request);
    let prefix = if path.starts_with("/api/xa401/konomi/recording") {
        "/api/xa401/konomi/recording/"
    } else {
        "/api/recording/"
    };
    let tail = path.strip_prefix(prefix).unwrap_or("").trim_matches('/');
    if tail == "presets" && matches!(method, Method::GET | Method::HEAD) {
        let defaults: Value =
            serde_json::from_str(include_str!("../templates/record-settings.json")).unwrap();
        return Json(json!({"global_defaults":{"recording_start_margin":0,"recording_end_margin":0,"caption_recording_mode":"Enable","data_broadcasting_recording_mode":"Enable","post_recording_mode":"Nothing"},"presets":[{"id":0,"name":"XA401 Bridge（PC録画）","record_settings":defaults}]})).into_response();
    }
    if tail != "reservations" && !tail.starts_with("reservations/") {
        return (StatusCode::NOT_IMPLEMENTED,Json(json!({"detail":"予約の作成・設定変更は /api/xa401/recordings のPC録画管理画面をご利用ください。"}))).into_response();
    }
    let requested = tail.strip_prefix("reservations/").map(|s| s.parse::<u64>());
    let requested = match requested {
        Some(Ok(n)) => Some(n),
        Some(Err(_)) => return StatusCode::NOT_FOUND.into_response(),
        None => None,
    };
    let jobs = a
        .recorder
        .snapshot()
        .into_iter()
        .filter(active)
        .collect::<Vec<_>>();
    if method == Method::DELETE {
        let job = jobs.iter().find(|j| id(j).ok() == requested);
        return match job
            .and_then(|j| j["id"].as_str())
            .and_then(|id| a.recorder.stop(id).ok())
        {
            Some(_) => StatusCode::NO_CONTENT.into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    if !matches!(method, Method::GET | Method::HEAD) {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"detail":"PC録画管理画面から予約を作成してください。"})),
        )
            .into_response();
    }
    let result: Result<Value> = async {
        let mut metadata = std::collections::HashMap::new();
        if !jobs.is_empty() {
            if let Ok(response) = crate::proxy::client()?
                .get("http://127.0.0.77:7010/api/channels")
                .send()
                .await
            {
                if let Ok(value) = crate::proxy::json_response(response, 8 * 1024 * 1024).await {
                    if let Some(groups) = value.as_object() {
                        for group in groups.values().filter_map(Value::as_array) {
                            for channel in group {
                                if let (Some(nid), Some(sid)) = (
                                    channel["network_id"].as_u64(),
                                    channel["service_id"].as_u64(),
                                ) {
                                    metadata.insert((nid, sid), channel.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
        let d = a.device.lock().await;
        let mut rows = vec![];
        let mut seen = std::collections::HashSet::new();
        for job in &jobs {
            ensure!(seen.insert(id(job)?), "Reservation identity collision");
            let service = d
                .items
                .iter()
                .filter_map(|i| i.service.as_ref())
                .find(|s| s["id"] == job["source_key"])
                .ok_or_else(|| anyhow::anyhow!("Reserved channel unavailable"))?;
            let c = metadata
                .get(&(
                    service["networkId"].as_u64().unwrap_or(0),
                    service["serviceId"].as_u64().unwrap_or(0),
                ))
                .cloned();
            rows.push(row(job, service, c, &jobs, a.config.max_streams)?);
        }
        rows.sort_by(|a, b| {
            a["program"]["start_time"]
                .as_str()
                .cmp(&b["program"]["start_time"].as_str())
                .then(a["id"].as_u64().cmp(&b["id"].as_u64()))
        });
        if let Some(id) = requested {
            return rows
                .into_iter()
                .find(|r| r["id"].as_u64() == Some(id))
                .ok_or_else(|| anyhow::anyhow!("Unknown reservation"));
        }
        Ok(json!({"total":rows.len(),"reservations":rows}))
    }
    .await;
    match result {
        Ok(v) => Json(v).into_response(),
        Err(_) => {
            if requested.is_some() {
                StatusCode::NOT_FOUND.into_response()
            } else {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Reservation metadata unavailable",
                )
                    .into_response()
            }
        }
    }
}
pub(crate) fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/recording/{*tail}", any(handler))
        .route("/api/xa401/konomi/recording/{*tail}", any(handler))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overlap_tracks_distinct_services() {
        let service = json!({"networkId":4,"serviceId":211,"transportStreamId":1,"name":"channel","channel":{"type":"BS"},"remoteControlKeyId":0});
        let job = json!({"id":"00000000000100000000000000000000","source":"live","source_key":400211,"start_at":946684800.,"duration_seconds":1800.,"title":"recording","status":"scheduled","file_name":"recording.ts"});
        let mut other = job.clone();
        other["id"] = json!("00000000000200000000000000000000");
        let value = row(&job, &service, None, &[job.clone(), other.clone()], 1).unwrap();
        assert_eq!(value["recording_availability"], "Full");
        other["source_key"] = json!(400212);
        assert_eq!(
            row(&job, &service, None, &[job.clone(), other], 1).unwrap()["recording_availability"],
            "Partial"
        );
    }
}
