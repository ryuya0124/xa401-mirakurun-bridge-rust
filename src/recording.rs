//! PC jobs only. No STB recording, Move, copy-count or delete operations.
use crate::{device::Config, server::App};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::{Mutex, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::AsyncWriteExt;
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
pub fn safe_title(title: &str) -> String {
    let s = title
        .chars()
        .filter(|c| !c.is_control() && !"<>:\"/\\|?*".contains(*c))
        .take(80)
        .collect::<String>()
        .trim_matches([' ', '.'])
        .to_string();
    if s.is_empty() { "recording".into() } else { s }
}
pub struct Recorder {
    closing: std::sync::atomic::AtomicBool,
    pub enabled: bool,
    pub directory: PathBuf,
    pub jobs: Mutex<BTreeMap<String, Value>>,
    tasks: Mutex<HashSet<String>>,
    path: PathBuf,
    udn: String,
    ffprobe: PathBuf,
    ffmpeg: PathBuf,
    max_bytes: u64,
    min_free: u64,
}
impl Recorder {
    pub fn new(c: &Config) -> Result<Self> {
        let settings = &c.recording;
        let enabled = settings["enabled"] == true;
        let directory = settings["directory"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or(c.state_dir.join("recordings"));
        let ffprobe = PathBuf::from(settings["ffprobe"].as_str().unwrap_or(""));
        let ffmpeg = PathBuf::from(settings["ffmpeg"].as_str().unwrap_or(""));
        let path = c.state_dir.join("recording-jobs.json");
        let mut jobs = BTreeMap::new();
        if enabled {
            ensure!(
                ffprobe.is_file() && ffmpeg.is_file(),
                "Explicit media verifier paths required"
            );
            std::fs::create_dir_all(&directory)?;
            match std::fs::metadata(&path) {
                Ok(m) => {
                    ensure!(m.len() <= 2 * 1024 * 1024, "Recording state too large");
                    let state: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
                    ensure!(state["udn"] == c.udn, "Recording state identity mismatch");
                    let stored = state["jobs"]
                        .as_array()
                        .filter(|a| a.len() <= 512)
                        .ok_or_else(|| anyhow::anyhow!("Invalid recording state"))?;
                    for job in stored {
                        let mut job = job.clone();
                        let id = job["id"]
                            .as_str()
                            .filter(|s| {
                                s.len() == 32
                                    && s.bytes()
                                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
                            })
                            .ok_or_else(|| anyhow::anyhow!("Invalid job identifier"))?
                            .to_string();
                        if matches!(
                            job["status"].as_str(),
                            Some("recording" | "stopping" | "finalizing")
                        ) {
                            job["status"] = json!("interrupted");
                            job["error_type"] = json!("ProcessRestarted");
                        }
                        jobs.insert(id, job);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(Self {
            closing: std::sync::atomic::AtomicBool::new(false),
            enabled,
            directory,
            jobs: Mutex::new(jobs),
            tasks: Mutex::new(HashSet::new()),
            path,
            udn: c.udn.clone(),
            ffprobe,
            ffmpeg,
            max_bytes: settings["max_file_bytes"]
                .as_u64()
                .unwrap_or(20 * 1024 * 1024 * 1024),
            min_free: settings["min_free_bytes"]
                .as_u64()
                .unwrap_or(10 * 1024 * 1024 * 1024),
        })
    }
    fn save_locked(&self, jobs: &BTreeMap<String, Value>) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec(&json!({"udn":self.udn,"jobs":jobs.values().collect::<Vec<_>>()}))?,
        )?;
        std::fs::rename(tmp, &self.path)?;
        Ok(())
    }
    pub fn snapshot(&self) -> Vec<Value> {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }
    fn update(&self, id: &str, change: impl FnOnce(&mut Value)) -> Result<()> {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        change(
            jobs.get_mut(id)
                .ok_or_else(|| anyhow::anyhow!("Unknown job"))?,
        );
        self.save_locked(&jobs)
    }
    pub fn get(&self, id: &str) -> Option<Value> {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }
    pub fn path(&self, job: &Value) -> Result<PathBuf> {
        let name = job["file_name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing filename"))?;
        let p = Path::new(name);
        ensure!(
            p.components().count() == 1
                && !name.contains(['/', '\\'])
                && p.extension().is_some_and(|s| s == "ts"),
            "Recording path left directory"
        );
        Ok(self.directory.join(name))
    }
    pub async fn create(&self, app: &App, request: Value) -> Result<Value> {
        ensure!(
            self.enabled && !self.closing.load(std::sync::atomic::Ordering::Acquire),
            "PC recording disabled or stopping"
        );
        ensure!(request.is_object(), "Expected object");
        let source = request["source"].as_str().unwrap_or("live");
        let (mut key, title) = if source == "live" {
            let id = request["service_id"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("Invalid service"))?;
            let d = app.device.lock().await;
            let item = d
                .items
                .iter()
                .find(|i| {
                    i.service
                        .as_ref()
                        .is_some_and(|s| s["id"].as_u64() == Some(id))
                })
                .ok_or_else(|| anyhow::anyhow!("Unknown service"))?;
            (json!(id), item.title.clone())
        } else if source == "recorded" {
            let token = request["recording_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid recording"))?;
            let item = app
                .recordings()
                .await?
                .into_iter()
                .find(|i| i.token == token)
                .ok_or_else(|| anyhow::anyhow!("Unknown recording"))?;
            (json!(format!("recorded:{token}")), item.title)
        } else {
            anyhow::bail!("Invalid source")
        };
        let when = if let Some(start) = request.get("start_at").filter(|v| !v.is_null()) {
            chrono::DateTime::parse_from_rfc3339(
                start
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Invalid start time"))?,
            )?
            .timestamp_millis() as f64
                / 1000.
        } else {
            now()
        }
        .max(now());
        let duration = match request.get("duration_seconds") {
            None => 1800,
            Some(v) => v
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("Invalid duration type"))?,
        };
        ensure!(
            (5..=86400).contains(&duration) && when <= now() + 365. * 86400.,
            "Invalid duration/start"
        );
        let mut random = [0; 16];
        openssl::rand::rand_bytes(&mut random)?;
        let id = hex::encode(random);
        if source == "recorded" {
            key = json!(format!("{}@0#{id}", key.as_str().unwrap()));
        }
        let title = safe_title(
            request["title"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(&title),
        );
        let date = chrono::DateTime::from_timestamp(when as i64, 0)
            .unwrap()
            .with_timezone(&chrono::FixedOffset::east_opt(9 * 3600).unwrap())
            .format("%Y%m%d-%H%M%S");
        let name = format!("{date}-{}-{title}.ts", &id[..8]);
        let job = json!({"id":id,"source":source,"source_key":key,"title":title,"file_name":name,"start_at":when,"duration_seconds":duration,"status":"scheduled","bytes":0,"created_at":now(),"error_type":null});
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(jobs.len() < 512, "Job limit reached");
        ensure!(
            jobs.values()
                .filter(|j| !matches!(
                    j["status"].as_str(),
                    Some("completed" | "stopped" | "cancelled" | "failed" | "interrupted")
                ))
                .count()
                < 64,
            "Active recording job limit reached"
        );
        jobs.insert(id, job.clone());
        self.save_locked(&jobs)?;
        Ok(job)
    }
    pub fn stop(&self, id: &str) -> Result<Value> {
        self.update(id, |j| {
            if !matches!(
                j["status"].as_str(),
                Some("completed" | "stopped" | "failed" | "cancelled" | "interrupted")
            ) {
                if self
                    .tasks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(id)
                {
                    j["stop_requested"] = json!(true);
                    j["status"] = json!("stopping");
                } else {
                    j["status"] = json!("cancelled");
                }
            }
        })?;
        Ok(self.get(id).unwrap())
    }
    pub fn start(app: Weak<App>) {
        tokio::spawn(async move {
            loop {
                let Some(app) = app.upgrade() else { break };
                if !app.recorder.enabled
                    || app
                        .recorder
                        .closing
                        .load(std::sync::atomic::Ordering::Acquire)
                {
                    break;
                }
                let r = app.recorder.clone();
                for j in r.snapshot() {
                    let id = j["id"].as_str().unwrap_or("").to_string();
                    if matches!(j["status"].as_str(), Some("scheduled" | "waiting"))
                        && j["start_at"].as_f64().unwrap_or(f64::MAX) <= now()
                    {
                        let inserted = r
                            .tasks
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(id.clone());
                        if inserted {
                            let a = app.clone();
                            let recorder = r.clone();
                            tokio::spawn(async move {
                                if let Err(error) = recorder.capture(&a, &id).await {
                                    let code = if error
                                        .chain()
                                        .any(|cause| cause.to_string() == "MediaVerificationFailed")
                                    {
                                        "MediaVerificationFailed"
                                    } else {
                                        "RecordingFailed"
                                    };
                                    let _ = recorder.update(&id, |j| {
                                        j["status"] = json!("failed");
                                        j["error_type"] = json!(code);
                                        j["updated_at"] = json!(now());
                                    });
                                    eprintln!("PC recording failed (details suppressed)");
                                }
                                recorder
                                    .tasks
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .remove(&id);
                            });
                        }
                    }
                }
                drop(app);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }
    async fn capture(&self, app: &App, id: &str) -> Result<()> {
        let job = self.get(id).ok_or_else(|| anyhow::anyhow!("Unknown job"))?;
        let destination = self.path(&job)?;
        let partial = destination.with_extension("part");
        let deadline =
            job["start_at"].as_f64().unwrap() + job["duration_seconds"].as_f64().unwrap();
        let mut reason = "duration";
        let (guard, mut rx) = loop {
            ensure!(
                now() < deadline,
                "Receive slots unavailable before deadline"
            );
            if self.get(id).is_some_and(|j| j["stop_requested"] == true) {
                self.update(id, |j| j["status"] = json!("cancelled"))?;
                return Ok(());
            }
            match app.subscribe_key(&job["source_key"]).await {
                Ok(pair) => break pair,
                Err((code, _)) if code == axum::http::StatusCode::SERVICE_UNAVAILABLE => {
                    self.update(id, |j| j["status"] = json!("waiting"))?;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(_) => anyhow::bail!("Receive startup failed"),
            }
        };
        ensure!(
            fs2::available_space(&self.directory)? >= self.min_free,
            "Insufficient recording space"
        );
        self.update(id, |j| {
            j["status"] = json!("recording");
            j["started_at"] = json!(now());
        })?;
        let mut writer = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .await?;
        let mut total = 0;
        let mut checked = 0.;
        while now() < deadline && !self.get(id).is_some_and(|j| j["stop_requested"] == true) {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Ok(chunk)) => {
                    if chunk.is_empty() {
                        ensure!(
                            guard.outcome.load(std::sync::atomic::Ordering::Acquire) != 2,
                            "Receive failed"
                        );
                        reason = "source_eof";
                        break;
                    }
                    ensure!(
                        chunk.len() % 188 == 0
                            && chunk
                                .chunks_exact(188)
                                .all(|p| p[0] == 0x47 && p[3] & 192 == 0),
                        "Invalid/scrambled TS"
                    );
                    ensure!(
                        total + chunk.len() as u64 <= self.max_bytes,
                        "Recording size limit reached"
                    );
                    writer.write_all(&chunk).await?;
                    total += chunk.len() as u64;
                    if now() - checked >= 2. {
                        ensure!(
                            fs2::available_space(&self.directory)? >= self.min_free,
                            "Recording disk full"
                        );
                        checked = now();
                        self.update(id, |j| j["bytes"] = json!(total))?;
                    }
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    reason = "source_eof";
                    break;
                }
                Ok(Err(_)) => anyhow::bail!("Recorder could not keep up"),
                Err(_) => continue,
            }
        }
        writer.sync_all().await?;
        drop(writer);
        drop(guard);
        ensure!(total > 0, "No TS received");
        ensure!(
            fs2::available_space(&self.directory)? >= total + self.min_free,
            "Insufficient finalization space"
        );
        self.update(id, |j| j["status"] = json!("finalizing"))?;
        let verification =
            crate::ts_file::finalize(&partial, &destination, &self.ffprobe, &self.ffmpeg)
                .await
                .context("MediaVerificationFailed")?;
        self.update(id, |j| {
            let stopped = j["stop_requested"] == true;
            j["status"] = json!(if stopped { "stopped" } else { "completed" });
            j["finish_reason"] = json!(if stopped { "user_stop" } else { reason });
            j["source_complete"] = json!(j["source"] == "recorded" && reason == "source_eof");
            j["verification"] = verification;
            j["bytes"] = json!(
                std::fs::metadata(destination)
                    .map(|m| m.len())
                    .unwrap_or(total)
            );
            j["updated_at"] = json!(now());
        })?;
        Ok(())
    }
    pub async fn close(&self) {
        self.closing
            .store(true, std::sync::atomic::Ordering::Release);
        let ids = self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        for id in ids {
            let _ = self.stop(&id);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(70);
        while tokio::time::Instant::now() < deadline {
            if self
                .tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
