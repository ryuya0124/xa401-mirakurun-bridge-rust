//! Native loopback API and bounded, same-channel shared receiving sessions.
use crate::{
    crypto::Credentials,
    device::{Config, Device, Item},
    receiver,
};
use anyhow::{Result, ensure};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Weak,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, broadcast};
pub(crate) type Shared = Arc<App>;
pub(crate) struct App {
    closing: std::sync::atomic::AtomicBool,
    pub(crate) config: Config,
    pub(crate) device: Mutex<Device>,
    credentials: Arc<Credentials>,
    sessions: Mutex<HashMap<String, Weak<Session>>>,
    recordings: Mutex<(u64, Vec<Item>)>,
    ake: Arc<Mutex<()>>,
    epg: Arc<std::sync::Mutex<crate::cache::Cache>>,
    pub(crate) collector_status: std::sync::Mutex<Value>,
    pub(crate) recorder: Arc<crate::recording::Recorder>,
}
pub(crate) struct Session {
    key: String,
    tx: broadcast::Sender<Bytes>,
    abort: tokio::task::AbortHandle,
    bytes: Arc<AtomicU64>,
    started: u64,
    pub(crate) outcome: Arc<AtomicU8>,
    viewers: AtomicU64,
    pub(crate) observations: Arc<AtomicU64>,
    done: Arc<tokio::sync::Notify>,
}
pub(crate) struct Subscription {
    session: Arc<Session>,
    viewing: bool,
}
impl std::ops::Deref for Subscription {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.session
    }
}
impl Subscription {
    fn new(session: Arc<Session>, background: bool) -> Self {
        if !background {
            session.viewers.fetch_add(1, Ordering::Relaxed);
        }
        Self {
            session,
            viewing: !background,
        }
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        if self.viewing {
            self.session.viewers.fetch_sub(1, Ordering::Relaxed);
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.abort.abort();
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub(crate) type ApiResult<T> = std::result::Result<T, (StatusCode, &'static str)>;
fn bad(_: anyhow::Error) -> (StatusCode, &'static str) {
    (
        StatusCode::BAD_GATEWAY,
        "XA401 operation failed; sensitive details suppressed",
    )
}
async fn boundary(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let valid = url::Url::parse(&format!("http://{host}"))
        .ok()
        .is_some_and(|u| {
            u.username().is_empty()
                && u.password().is_none()
                && u.path() == "/"
                && u.host_str().is_some_and(|h| {
                    h == "localhost"
                        || h.parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                })
        });
    if !valid {
        return (StatusCode::MISDIRECTED_REQUEST, "Loopback Host required").into_response();
    }
    let h = req.headers();
    let path = req.uri().path();
    let html = matches!(
        path,
        "/api/xa401/recordings" | "/api/xa401/original" | "/api/xa401/desktop-player"
    );
    let navigation = req.method() == axum::http::Method::GET
        && h.get("sec-fetch-mode").is_some_and(|v| v == "navigate")
        && h.get("sec-fetch-dest").is_some_and(|v| v == "document")
        && (!path.starts_with("/api/") && !path.starts_with("/xa401/api/") || html);
    if !navigation
        && (h.get("sec-fetch-site").is_some_and(|v| v == "cross-site")
            || h.get("origin")
                .is_some_and(|v| v.to_str().ok() != Some(format!("http://{host}").as_str())))
    {
        return (StatusCode::FORBIDDEN, "Cross-origin request refused").into_response();
    }
    next.run(req).await
}
impl App {
    pub(crate) async fn recordings(&self) -> Result<Vec<Item>> {
        let mut cache = self.recordings.lock().await;
        if now().saturating_sub(cache.0) > 60 {
            cache.1 = self.device.lock().await.recordings().await?;
            cache.0 = now();
        }
        Ok(cache.1.clone())
    }
    pub(crate) fn programs(&self) -> Vec<Value> {
        self.epg
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .programs()
    }
    pub(crate) async fn subscribe_key(
        &self,
        key: &Value,
    ) -> ApiResult<(Subscription, broadcast::Receiver<Bytes>)> {
        if let Some(id) = key.as_u64() {
            let object = self
                .device
                .lock()
                .await
                .items
                .iter()
                .find(|i| {
                    i.service
                        .as_ref()
                        .is_some_and(|s| s["id"].as_u64() == Some(id))
                })
                .map(|i| i.object_id.clone())
                .ok_or((StatusCode::NOT_FOUND, "Unknown service"))?;
            return self.subscribe(id.to_string(), object, 0.).await;
        }
        if let Some(key) = key.as_str() {
            if let Some(rest) = key.strip_prefix("recorded:") {
                let token = rest.split('@').next().unwrap_or("");
                let item = self
                    .recordings()
                    .await
                    .map_err(bad)?
                    .into_iter()
                    .find(|i| i.token == token)
                    .ok_or((StatusCode::NOT_FOUND, "Recording unavailable"))?;
                return self.subscribe(key.to_string(), item.object_id, 0.).await;
            }
        }
        Err((StatusCode::BAD_REQUEST, "Unknown source"))
    }
    async fn subscribe(
        &self,
        key: String,
        object_id: String,
        position: f64,
    ) -> ApiResult<(Subscription, broadcast::Receiver<Bytes>)> {
        self.subscribe_inner(key, object_id, position, false).await
    }
    async fn subscribe_inner(
        &self,
        key: String,
        object_id: String,
        position: f64,
        background: bool,
    ) -> ApiResult<(Subscription, broadcast::Receiver<Bytes>)> {
        if self.closing.load(Ordering::Acquire) {
            return Err((StatusCode::SERVICE_UNAVAILABLE, "Bridge is stopping"));
        }
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, v| v.strong_count() > 0);
        if let Some(s) = sessions.get(&key).and_then(Weak::upgrade) {
            let rx = s.tx.subscribe();
            return Ok((Subscription::new(s, background), rx));
        }
        if sessions.len() >= self.config.max_streams && !background {
            let victim = sessions.iter().find_map(|(key, weak)| {
                weak.upgrade()
                    .filter(|s| s.viewers.load(Ordering::Relaxed) == 0)
                    .map(|s| (key.clone(), s))
            });
            if let Some((key, victim)) = victim {
                sessions.remove(&key);
                victim.abort.abort();
                let _ = tokio::time::timeout(Duration::from_secs(2), victim.done.notified()).await;
            }
        }
        if sessions.len() >= self.config.max_streams {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "All receive slots are in use",
            ));
        }
        let mut device = self.device.lock().await;
        let resource = device.resource(&object_id).await.map_err(bad)?;
        let host = device.host;
        let mut sit = device
            .items
            .iter()
            .find(|i| i.object_id == object_id)
            .and_then(|i| i.service.clone())
            .map(crate::epg::Parser::new);
        drop(device);
        let (tx, rx) = broadcast::channel(32);
        let publish = tx.clone();
        let (input, mut output) = tokio::sync::mpsc::channel(8);
        let credentials = self.credentials.clone();
        let source = self.config.source;
        let path = self.config.state_dir.join("dtcp/srm.bin");
        let bytes = Arc::new(AtomicU64::new(0));
        let metric = bytes.clone();
        let outcome = Arc::new(AtomicU8::new(0));
        let finished = outcome.clone();
        let observations = Arc::new(AtomicU64::new(0));
        let observed = observations.clone();
        let done = Arc::new(tokio::sync::Notify::new());
        struct Done(Arc<tokio::sync::Notify>, Arc<AtomicU8>);
        impl Drop for Done {
            fn drop(&mut self) {
                let _ = self
                    .1
                    .compare_exchange(0, 3, Ordering::AcqRel, Ordering::Acquire);
                self.0.notify_one();
            }
        }
        let signal = Done(done.clone(), outcome.clone());
        let ake = self.ake.clone();
        let epg = self.epg.clone();
        let task = tokio::spawn(async move {
            let _signal = signal;
            // The authentication lock is released before reading the media body.
            let worker = tokio::spawn(async move {
                receiver::receive_locked(
                    source,
                    host,
                    &resource,
                    &credentials,
                    &path,
                    position,
                    input,
                    Some(ake),
                )
                .await
            });
            struct Cancel(tokio::task::AbortHandle);
            impl Drop for Cancel {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let _cancel = Cancel(worker.abort_handle());
            while let Some(chunk) = output.recv().await {
                if let Some(ref mut sit) = sit {
                    let programs = sit.feed(&chunk);
                    observed.fetch_add(programs.len() as u64, Ordering::Relaxed);
                    if !programs.is_empty() {
                        let mut cache = epg.lock().unwrap_or_else(|e| e.into_inner());
                        let mut changed = false;
                        for p in programs {
                            changed |= cache.update(p).unwrap_or(false);
                        }
                        if changed && cache.save().is_err() {
                            eprintln!("Observed EPG cache write failed");
                        }
                    }
                }
                metric.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                if publish.send(chunk).is_err() {
                    break;
                }
            }
            let failed = !matches!(worker.await, Ok(Ok(())));
            finished.store(if failed { 2 } else { 1 }, Ordering::Release);
            if failed {
                eprintln!("Native receive session failed (details suppressed)");
            }
            let _ = publish.send(Bytes::new());
        });
        let s = Arc::new(Session {
            key: key.clone(),
            tx,
            abort: task.abort_handle(),
            bytes,
            started: now(),
            outcome,
            viewers: AtomicU64::new(0),
            observations,
            done,
        });
        sessions.insert(key, Arc::downgrade(&s));
        Ok((Subscription::new(s, background), rx))
    }
}
impl App {
    fn current_program(&self, id: u64, at: u64) -> Option<Value> {
        self.programs().into_iter().find(|p| {
            p["networkId"].as_u64().unwrap_or(0) * 100000 + p["serviceId"].as_u64().unwrap_or(0)
                == id
                && p["startAt"].as_u64().is_some_and(|start| {
                    start <= at * 1000 && at * 1000 < start + p["duration"].as_u64().unwrap_or(0)
                })
        })
    }
    fn collector_count(&self, field: &str) {
        let mut s = self
            .collector_status
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        s[field] = json!(s[field].as_u64().unwrap_or(0) + 1);
    }
    fn start_collector(app: Weak<App>) {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let mut due = HashMap::<u64, u64>::new();
            let mut failures = HashMap::<u64, u32>::new();
            let mut refreshed = now();
            loop {
                let Some(a) = app.upgrade() else { break };
                if a.closing.load(Ordering::Acquire) {
                    break;
                }
                let settings = &a.config.epg_collection;
                let enabled = settings["enabled"] == true;
                let interval = settings["interval_seconds"]
                    .as_f64()
                    .unwrap_or(2.)
                    .clamp(2., 60.);
                let refresh = settings["refresh_seconds"]
                    .as_u64()
                    .unwrap_or(600)
                    .clamp(120, 3600);
                let limit = settings["timeout_seconds"]
                    .as_f64()
                    .unwrap_or(10.)
                    .clamp(3., 20.);
                if now().saturating_sub(refreshed) >= 300
                    && a.sessions
                        .lock()
                        .await
                        .values()
                        .all(|s| s.strong_count() == 0)
                {
                    if a.device.lock().await.refresh().await.is_ok() {
                        refreshed = now();
                    }
                }
                if enabled {
                    let types = settings["types"]
                        .as_array()
                        .cloned()
                        .unwrap_or_else(|| vec![json!("GR"), json!("BS"), json!("CS")]);
                    let mut candidates = vec![];
                    {
                        let d = a.device.lock().await;
                        for item in &d.items {
                            if let Some(s) = &item.service {
                                if !types.contains(&s["channel"]["type"]) {
                                    continue;
                                }
                                let id = s["id"].as_u64().unwrap_or(0);
                                let when = *due.entry(id).or_insert_with(|| {
                                    a.current_program(id, now())
                                        .map(|p| {
                                            (now() + refresh).min(
                                                (p["startAt"].as_u64().unwrap_or(0)
                                                    + p["duration"].as_u64().unwrap_or(0))
                                                    / 1000
                                                    + 1,
                                            )
                                        })
                                        .unwrap_or(0)
                                });
                                if when <= now() {
                                    let rank = match s["channel"]["type"].as_str() {
                                        Some("GR") => 0,
                                        Some("BS") => 1,
                                        _ => 2,
                                    };
                                    candidates.push((
                                        rank,
                                        when,
                                        s["remoteControlKeyId"].as_u64().unwrap_or(0),
                                        id,
                                        item.object_id.clone(),
                                    ));
                                }
                            }
                        }
                    }
                    candidates.sort();
                    if let Some((_, _, _, id, object)) = candidates.into_iter().next() {
                        a.collector_status.lock().unwrap_or_else(|e| e.into_inner())["active_service"] =
                            json!(id);
                        a.collector_count("attempted");
                        let mut result = "failed";
                        match a.subscribe_inner(id.to_string(), object, 0., true).await {
                            Ok((guard, mut rx)) => {
                                let deadline =
                                    tokio::time::Instant::now() + Duration::from_secs_f64(limit);
                                result = "timeout";
                                while tokio::time::Instant::now() < deadline {
                                    if guard.outcome.load(Ordering::Acquire) == 3 {
                                        result = "preempted";
                                        break;
                                    }
                                    match tokio::time::timeout(Duration::from_secs(2), rx.recv())
                                        .await
                                    {
                                        Ok(Ok(chunk)) if !chunk.is_empty() => {
                                            if guard.observations.load(Ordering::Relaxed) > 0
                                                && a.current_program(id, now()).is_some()
                                            {
                                                result = "success";
                                                break;
                                            }
                                        }
                                        Ok(Err(_)) | Ok(Ok(_)) => {
                                            result = if guard.outcome.load(Ordering::Acquire) == 3 {
                                                "preempted"
                                            } else {
                                                "failed"
                                            };
                                            break;
                                        }
                                        Err(_) => continue,
                                    }
                                }
                            }
                            Err((StatusCode::SERVICE_UNAVAILABLE, _)) => result = "busy",
                            Err(_) => (),
                        }
                        if result == "success" {
                            failures.remove(&id);
                            a.collector_count("completed");
                            let when = a
                                .current_program(id, now())
                                .map(|p| {
                                    (now() + refresh).min(
                                        (p["startAt"].as_u64().unwrap_or(0)
                                            + p["duration"].as_u64().unwrap_or(0))
                                            / 1000
                                            + 1,
                                    )
                                })
                                .unwrap_or(now() + 30);
                            due.insert(id, when);
                        } else if result == "busy" || result == "preempted" {
                            if result == "preempted" {
                                a.collector_count("preempted");
                            }
                            due.insert(id, now() + 5);
                        } else {
                            let f = failures.entry(id).or_insert(0);
                            *f = (*f + 1).min(5);
                            due.insert(id, now() + (300 * (1u64 << (*f - 1))).min(3600));
                        }
                        a.collector_status.lock().unwrap_or_else(|e| e.into_inner())["active_service"] =
                            Value::Null;
                    }
                }
                drop(a);
                tokio::time::sleep(Duration::from_secs_f64(interval)).await;
            }
        });
    }
}
async fn deliver(s: Subscription, mut rx: broadcast::Receiver<Bytes>) -> ApiResult<Response> {
    let first = tokio::time::timeout(Duration::from_secs(35), rx.recv())
        .await
        .map_err(|_| (StatusCode::GATEWAY_TIMEOUT, "XA401 startup timeout"))?
        .map_err(|_| {
            (
                StatusCode::BAD_GATEWAY,
                "XA401 did not produce validated TS",
            )
        })?;
    if first.is_empty() {
        return Err((
            StatusCode::BAD_GATEWAY,
            "XA401 did not produce validated TS",
        ));
    }
    let stream = futures_util::stream::unfold(
        (Some(first), rx, s),
        |(mut first, mut rx, guard)| async move {
            if let Some(chunk) = first.take() {
                return Some((Ok::<_, std::io::Error>(chunk), (None, rx, guard)));
            }
            match tokio::time::timeout(Duration::from_secs(20), rx.recv()).await {
                Ok(Ok(chunk)) if !chunk.is_empty() => Some((Ok(chunk), (None, rx, guard))),
                _ => None,
            }
        },
    );
    Ok((
        [
            ("Content-Type", "video/MP2T"),
            ("Cache-Control", "no-store"),
            ("X-Content-Type-Options", "nosniff"),
            ("X-XA401-Engine", "rust"),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}
async fn live(State(a): State<Shared>, Path(id): Path<u64>) -> ApiResult<Response> {
    let object = a
        .device
        .lock()
        .await
        .items
        .iter()
        .find(|i| {
            i.service
                .as_ref()
                .is_some_and(|s| s["id"].as_u64() == Some(id))
        })
        .map(|i| i.object_id.clone())
        .ok_or((StatusCode::NOT_FOUND, "Unknown service"))?;
    let (s, rx) = a.subscribe(id.to_string(), object, 0.).await?;
    deliver(s, rx).await
}
async fn recorded(
    State(a): State<Shared>,
    Path(token): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let player = q.get("player").cloned().unwrap_or_default();
    let position = q
        .get("position")
        .map(|v| v.parse::<f64>())
        .transpose()
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid position"))?
        .unwrap_or(0.);
    if token.len() != 24
        || !token
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        || player.len() != 32
        || !player
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        || !position.is_finite()
        || !(0. ..=86400.).contains(&position)
    {
        return Err((StatusCode::BAD_REQUEST, "Invalid playback request"));
    }
    let item = a
        .recordings()
        .await
        .map_err(bad)?
        .into_iter()
        .find(|i| i.token == token)
        .ok_or((StatusCode::NOT_FOUND, "Recording unavailable"))?;
    // Stop an old request from this player before acquiring its replacement slot.
    {
        let mut sessions = a.sessions.lock().await;
        let suffix = format!("#{player}");
        sessions.retain(|k, v| {
            if k.starts_with("recorded:") && k.ends_with(&suffix) {
                if let Some(s) = v.upgrade() {
                    s.abort.abort();
                }
                false
            } else {
                true
            }
        });
    }
    let (s, rx) = a
        .subscribe(
            format!("recorded:{token}@{position:.3}#{player}"),
            item.object_id,
            position,
        )
        .await?;
    deliver(s, rx).await
}
fn duration(text: Option<&str>) -> Option<f64> {
    let mut p = text?.split(':');
    let h = p.next()?.parse::<f64>().ok()?;
    let m = p.next()?.parse::<f64>().ok()?;
    let s = p.next()?.parse::<f64>().ok()?;
    let t = h * 3600. + m * 60. + s;
    if t.is_finite() && t > 0. && t <= 86400. {
        Some(t)
    } else {
        None
    }
}
async fn catalogue(State(a): State<Shared>) -> ApiResult<Json<Value>> {
    let recordings=a.recordings().await.map_err(bad)?.into_iter().map(|i|json!({"id":i.token,"title":i.title,"channel":i.channel,"date":i.date,"duration":i.duration,"duration_seconds":duration(i.duration.as_deref())})).collect::<Vec<_>>();
    let channels=a.device.lock().await.items.iter().filter_map(|i|i.service.as_ref()).map(|s|json!({"id":s["id"].as_u64().unwrap_or(0).to_string(),"title":s["name"],"channel":s["channel"]["type"]})).collect::<Vec<_>>();
    Ok(Json(
        json!({"protocol":1,"recordings":recordings,"channels":channels}),
    ))
}
async fn source(State(a): State<Shared>, Path(id): Path<u64>) -> ApiResult<Json<Value>> {
    let d = a.device.lock().await;
    let item = d
        .items
        .iter()
        .find(|i| {
            i.service
                .as_ref()
                .is_some_and(|s| s["id"].as_u64() == Some(id))
        })
        .ok_or((StatusCode::NOT_FOUND, "Unknown service"))?;
    Ok(Json(
        json!({"kind":"live","id":id.to_string(),"title":item.service.as_ref().unwrap()["name"],"duration":null,"stream_url":format!("/xa401/api/native/channels/{id}/stream")}),
    ))
}
async fn recorded_source(
    State(a): State<Shared>,
    Path(token): Path<String>,
) -> ApiResult<Json<Value>> {
    let item = a
        .recordings()
        .await
        .map_err(bad)?
        .into_iter()
        .find(|i| i.token == token)
        .ok_or((StatusCode::NOT_FOUND, "Recording unavailable"))?;
    let mut d = a.device.lock().await;
    let resource = d.resource(&item.object_id).await.map_err(bad)?;
    let head = d
        .http
        .head(&resource.url)
        .header("TimeSeekRange.dlna.org", "npt=0.000-")
        .header("transferMode.dlna.org", "Streaming")
        .header("getcontentFeatures.dlna.org", "1")
        .header("Accept-Encoding", "identity")
        .send()
        .await
        .map_err(|_| (StatusCode::BAD_GATEWAY, "Duration probe failed"))?;
    let seconds = head
        .headers()
        .get("TimeSeekRange.dlna.org")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split('/').nth(1))
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s > 0. && *s <= 86400.)
        .ok_or((StatusCode::BAD_GATEWAY, "Precise duration unavailable"))?;
    Ok(Json(
        json!({"kind":"recorded","id":token,"title":item.title,"duration":seconds,"stream_url":format!("/xa401/api/recorded/{token}/stream")}),
    ))
}
pub(crate) async fn recorded_source_value(a: &Shared, token: &str) -> Result<Value> {
    recorded_source(State(a.clone()), Path(token.to_string()))
        .await
        .map(|Json(v)| v)
        .map_err(|_| anyhow::anyhow!("Precise recording duration unavailable"))
}
async fn guide(State(a): State<Shared>) -> Json<Value> {
    let programs = a.programs();
    let channels=a.device.lock().await.items.iter().filter_map(|i|i.service.as_ref()).map(|s|{
        let program=programs.iter().find(|p|p["networkId"]==s["networkId"]&&p["serviceId"]==s["serviceId"]).map(|p|json!({"event_id":p["eventId"],"title":p["name"],"description":p["description"],"extended":p["extended"],"start_at":p["startAt"],"duration":p["duration"]}));
        json!({"id":s["id"].as_u64().unwrap_or(0).to_string(),"name":s["name"],"type":s["channel"]["type"],"number":s["remoteControlKeyId"],"program":program})
    }).collect::<Vec<_>>();
    Json(
        json!({"protocol":1,"generated_at":now()*1000,"source":"observed_SIT","future_schedule_available":false,"channels":channels}),
    )
}
async fn health(State(a): State<Shared>) -> Json<Value> {
    let streams=a.sessions.lock().await.values().filter_map(Weak::upgrade).map(|s|json!({"service_id":s.key,"ts_bytes":s.bytes.load(Ordering::Relaxed),"clients":s.tx.receiver_count(),"viewers":s.viewers.load(Ordering::Relaxed),"started_at":s.started})).collect::<Vec<_>>();
    Json(
        json!({"status":"ok","engine":"rust","openssl":openssl::version::version(),"services":a.device.lock().await.items.len(),"streams":streams,"maximum_distinct_channels":a.config.max_streams,"cached_programs":a.programs().len(),"epg":"observed_SIT","epg_collection":a.collector_status.lock().unwrap_or_else(|e|e.into_inner()).clone()}),
    )
}
fn tuners_response(max_streams: usize, count: usize) -> Response {
    (
        [(
            axum::http::header::SERVER,
            concat!("xa401-mirakurun-bridge/", env!("CARGO_PKG_VERSION")),
        )],
        Json((0..max_streams).map(|i|json!({"index":i,"name":format!("XA401 receive slot {}",i+1),"types":["GR","BS","CS","CATV"],"isAvailable":true,"isFree":i>=count,"isRemote":false,"users":if i<count{vec![json!({"id":format!("xa401-native-{i}"),"priority":0})]}else{vec![]}})).collect::<Vec<_>>()),
    )
        .into_response()
}
pub async fn serve(config: Config) -> Result<()> {
    ensure!(
        config.listen.is_loopback(),
        "Only loopback API binding is permitted"
    );
    let credentials = Arc::new(Credentials::load(&config.credentials)?);
    let mut device = Device::new(config.clone())?;
    device.refresh().await?;
    let bind = (config.listen, config.port);
    let epg = Arc::new(std::sync::Mutex::new(crate::cache::Cache::load(
        &config.state_dir,
        &config.udn,
    )));
    let recorder = Arc::new(crate::recording::Recorder::new(&config)?);
    let a = Arc::new(App {
        closing: std::sync::atomic::AtomicBool::new(false),
        config: config.clone(),
        device: Mutex::new(device),
        credentials,
        sessions: Mutex::new(HashMap::new()),
        recordings: Mutex::new((0, vec![])),
        ake: Arc::new(Mutex::new(())),
        epg,
        collector_status: std::sync::Mutex::new(
            json!({"enabled":config.epg_collection["enabled"]==true,"active_service":null,"attempted":0,"completed":0,"preempted":0}),
        ),
        recorder,
    });
    crate::recording::Recorder::start(Arc::downgrade(&a));
    App::start_collector(Arc::downgrade(&a));
    let app=Router::new()
        .route("/health",get(health))
        .route("/api/services",get(|State(a):State<Shared>|async move{Json(a.device.lock().await.items.iter().filter_map(|i|i.service.clone()).collect::<Vec<_>>())}))
        .route("/api/programs",get(|State(a):State<Shared>|async move{Json(a.programs())}))
        .route("/api/tuners",get(|State(a):State<Shared>|async move{let count=a.sessions.lock().await.values().filter(|s|s.strong_count()>0).count();tuners_response(a.config.max_streams,count)}))
        .route("/api/version",get(||async{Json(json!({"current":env!("CARGO_PKG_VERSION"),"name":"xa401-mirakurun-bridge","engine":"rust","compatibleApi":"Mirakurun subset"}))}))
        .route("/api/services/{id}/stream",get(live).head(||async{StatusCode::METHOD_NOT_ALLOWED}))
        .route("/xa401/api/native/catalogue",get(catalogue))
        .route("/xa401/api/native/guide",get(guide))
        .route("/xa401/api/native/channels/{id}",get(source))
        .route("/xa401/api/native/channels/{id}/stream",get(live).head(||async{StatusCode::METHOD_NOT_ALLOWED}))
        .route("/xa401/api/native/recordings/{token}",get(recorded_source))
        .route("/xa401/api/recorded/{token}/stream",get(recorded).head(||async{StatusCode::METHOD_NOT_ALLOWED}))
        .merge(crate::recording_api::routes())
        .merge(crate::konomi::routes())
        .merge(crate::reservations::routes())
        .fallback(crate::proxy::forward)
        .layer(middleware::from_fn(crate::proxy::frontend))
        .layer(middleware::from_fn(boundary)).with_state(a.clone());
    axum::serve(tokio::net::TcpListener::bind(bind).await?, app)
        .with_graceful_shutdown(async move {
            #[cfg(unix)]
            {
                if let Ok(mut term) =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                {
                    tokio::select! {_=tokio::signal::ctrl_c()=>(),_=term.recv()=>()}
                } else {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
            a.closing.store(true, Ordering::Release);
            a.recorder.close().await;
            for session in a.sessions.lock().await.values().filter_map(Weak::upgrade) {
                session.abort.abort();
            }
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tuner_probe_satisfies_konomitv_startup_validation() {
        let response = tuners_response(2, 1);
        assert_eq!(response.status(), StatusCode::OK);
        let server = response.headers()[axum::http::header::SERVER]
            .to_str()
            .unwrap();
        assert_eq!(server.split('/').next(), Some("xa401-mirakurun-bridge"));
        assert_eq!(server.split('/').nth(1), Some(env!("CARGO_PKG_VERSION")));
        let bytes = axum::body::to_bytes(response.into_body(), 16384)
            .await
            .unwrap();
        let tuners: Vec<Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(tuners.len(), 2);
        assert_eq!(tuners[0]["isFree"], false);
        assert_eq!(tuners[1]["isFree"], true);
    }
}
