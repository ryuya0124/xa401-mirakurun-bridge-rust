import { Mpeg2TsPlayer } from "/xa401/player-lib/index.js?rev=9cde78d";
import {
  RecordingTimeline,
  durationSeconds,
  clock,
} from "/xa401/timeline.mjs?v=20261115-2";
import { PlaybackSession } from "/xa401/playback-session.mjs?v=1";
const $ = (id) => document.getElementById(id);
let state = {},
  loaded = false;
const names = {
  scheduled: "予約済み",
  waiting: "受信枠待ち",
  recording: "録画中",
  finalizing: "保存・検証中",
  stopping: "停止処理中",
  completed: "保存完了",
  stopped: "停止・保存済み",
  cancelled: "予約取消",
  failed: "失敗",
  interrupted: "再起動等で中断",
};
function node(tag, text) {
  let n = document.createElement(tag);
  n.textContent = text;
  return n;
}
async function api(path, data) {
  let r = await fetch("/xa401/api/" + path, {
    cache: "no-store",
    ...(data
      ? {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(data),
        }
      : {}),
  });
  if (!r.ok) throw Error(await r.text());
  return await r.json();
}
function message(text) {
  $("message").textContent = text;
}
function titleFor(c) {
  let now = Date.now();
  let p = (state.programs || []).find(
    (p) =>
      p.networkId * 100000 + p.serviceId === c.id &&
      p.startAt <= now &&
      now < p.startAt + p.duration,
  );
  return p ? p.name : c.title;
}
function selectTitle() {
  let c = (state.channels || []).find(
    (c) => c.id === Number($("channel").value),
  );
  if (c) $("title").value = titleFor(c);
}
async function refresh() {
  try {
    state = await api("recording");
    if (!loaded) {
      for (let c of state.channels) {
        let o = node("option", c.name);
        o.value = c.id;
        $("channel").append(o);
      }
      selectTitle();
      loaded = true;
    }
    if (!state.enabled)
      message("PC録画はサーバー側でまだ有効になっていません。");
    renderJobs();
  } catch (e) {
    message(e.message);
  }
}
function renderJobs() {
  $("job-list").replaceChildren();
  for (let j of state.jobs) {
    let card = node("div", "");
    card.className = "card";
    card.append(
      node("strong", j.title),
      node(
        "p",
        `${names[j.status] || j.status} · ${new Date(j.start_at * 1000).toLocaleString()} · ${(j.bytes / 1024 ** 2).toFixed(1)} MiB`,
      ),
    );
    if (["scheduled", "waiting", "recording", "stopping"].includes(j.status)) {
      let b = node(
        "button",
        j.status === "scheduled" ? "予約を取り消す" : "停止して保存",
      );
      b.onclick = async () => {
        try {
          await api("jobs/" + j.id + "/stop", {});
          await refresh();
        } catch (e) {
          message(e.message);
        }
      };
      card.append(b);
    }
    if (["completed", "stopped"].includes(j.status)) {
      let a = node("a", "TSファイルを保存");
      a.href = "/xa401/api/jobs/" + j.id + "/file";
      a.download = j.file_name;
      card.append(
        a,
        node(
          "p",
          "KonomiTVへの反映に少し時間がかかることがあります。1分未満の動画はKonomiTVの対象外です。",
        ),
      );
    }
    if (j.error_type)
      card.append(
        node(
          "p",
          "処理に失敗しました。受信枠・空き容量・ローカルログを確認してください。",
        ),
      );
    $("job-list").append(card);
  }
}
$("channel").onchange = selectTitle;
$("live").onsubmit = async (e) => {
  e.preventDefault();
  try {
    let d = {
      source: "live",
      service_id: Number($("channel").value),
      title: $("title").value,
      duration_seconds: Math.round(Number($("minutes").value) * 60),
    };
    if ($("when").value) d.start_at = new Date($("when").value).toISOString();
    await api("jobs", d);
    message("録画・予約を登録しました。");
    await refresh();
    location.hash = "jobs";
  } catch (e) {
    message(e.message);
  }
};
let selected = null,
  playerId = crypto.randomUUID().replaceAll("-", "");
let timeline = null,
  dragging = false,
  generation = 0,
  readyGeneration = 0;
function showPosition(value) {
  if (!timeline) return;
  if (!dragging) $("timeline").value = String(value);
  $("clock").textContent =
    clock(value) +
    " / " +
    (timeline.duration ? clock(timeline.duration) : "時間未取得");
}
const video = $("video");
const session = new PlaybackSession(() => {
  const player = new Mpeg2TsPlayer(video, {
    mediaSource: "auto",
    passthrough: false,
  });
  player.addEventListener("error", (e) =>
    message("再生エラー: " + e.detail.error.message),
  );
  return player;
});
let frameCallback = null;
function holdFrame() {
  if (frameCallback !== null) video.cancelVideoFrameCallback?.(frameCallback);
  frameCallback = null;
  const canvas = $("held-frame");
  if (video.readyState < 2 || !video.videoWidth) return;
  try {
    canvas.width = Math.min(1920, video.videoWidth);
    canvas.height = Math.round(
      (canvas.width * video.videoHeight) / video.videoWidth,
    );
    canvas.getContext("2d").drawImage(video, 0, 0, canvas.width, canvas.height);
    canvas.hidden = false;
  } catch (_) {
    /* Frame retention is optional. */
  }
}
function releaseFrame() {
  const current = generation;
  const release = () => {
    frameCallback = null;
    if (current === generation && readyGeneration === current)
      $("held-frame").hidden = true;
  };
  if (video.requestVideoFrameCallback)
    frameCallback = video.requestVideoFrameCallback(release);
  else requestAnimationFrame(() => requestAnimationFrame(release));
}
function mediaPosition() {
  if (!timeline || !navigator.mediaSession) return;
  try {
    if (timeline.duration)
      navigator.mediaSession.setPositionState({
        duration: timeline.duration,
        playbackRate: video.playbackRate,
        position: timeline.position(video.currentTime),
      });
    navigator.mediaSession.playbackState = video.paused ? "paused" : "playing";
  } catch (_) {
    /* Older browsers may not implement position state. */
  }
}
function mediaTitle(r) {
  document.title = r.title;
  video.title = r.title;
  video.setAttribute("aria-label", r.title);
  if (navigator.mediaSession && typeof MediaMetadata !== "undefined") {
    navigator.mediaSession.metadata = new MediaMetadata({
      title: r.title,
      artist: r.channel || "XA401",
      album: "XA401の録画",
    });
  }
}
async function playRecorded(r, position = 0) {
  const sameRecording = selected?.id === r.id;
  if (sameRecording) holdFrame();
  else $("held-frame").hidden = true;
  const current = ++generation;
  readyGeneration = 0;
  selected = r;
  if (!sameRecording) mediaTitle(r);
  timeline = new RecordingTimeline(durationSeconds(r.duration), position);
  $("timeline").max = String(timeline.duration || 0);
  $("timeline").disabled = !timeline.duration;
  $("playback").hidden = false;
  $("playing-title").textContent = r.title;
  showPosition(timeline.offset);
  try {
    message("XA401に接続しています…");
    const active = await session.load(
      `/xa401/api/recorded/${r.id}/stream?player=${playerId}&position=${timeline.offset}`,
    );
    if (!active || generation !== current) return;
    readyGeneration = current;
    message("XA401から直接再生中");
    await $("video").play();
  } catch (e) {
    if (generation === current)
      message(
        "再生を開始できませんでした。再生ボタン、または位置を選び直してお試しください。",
      );
  }
}
$("video").addEventListener("playing", () => {
  if (timeline && readyGeneration === generation)
    timeline.start($("video").currentTime);
  $("toggle-play").textContent = "一時停止";
  if (readyGeneration === generation) releaseFrame();
  mediaPosition();
});
$("video").addEventListener("pause", () => {
  $("toggle-play").textContent = "再生";
  mediaPosition();
});
$("video").addEventListener("timeupdate", () => {
  if (timeline && readyGeneration === generation && !dragging)
    showPosition(timeline.position($("video").currentTime));
  if (readyGeneration === generation) mediaPosition();
});
$("video").addEventListener("ended", () => {
  if (timeline) showPosition(timeline.duration);
});
$("timeline").onpointerdown = () => {
  dragging = true;
};
$("timeline").oninput = () => {
  dragging = true;
  showPosition(Number($("timeline").value));
};
$("timeline").onchange = () => {
  dragging = false;
  if (selected && timeline)
    playRecorded(selected, timeline.target($("timeline").value));
};
$("toggle-play").onclick = async () => {
  try {
    if ($("video").paused) await $("video").play();
    else $("video").pause();
  } catch (_) {
    if (selected && timeline)
      playRecorded(selected, timeline.position($("video").currentTime));
  }
};
function skip(delta) {
  if (selected && timeline)
    playRecorded(
      selected,
      timeline.target(timeline.position($("video").currentTime) + delta),
    );
}
$("backward").onclick = () => skip(-10);
$("forward").onclick = () => skip(30);
$("volume").oninput = () => {
  $("video").volume = Number($("volume").value);
};
$("fullscreen").onclick = () => $("playback").requestFullscreen?.();
function stopPlayback() {
  ++generation;
  session.destroy();
  selected = null;
  timeline = null;
  if (frameCallback !== null) video.cancelVideoFrameCallback?.(frameCallback);
  frameCallback = null;
  $("held-frame").hidden = true;
  $("playback").hidden = true;
  document.title = "XA401 録画管理";
  if (navigator.mediaSession) {
    navigator.mediaSession.metadata = null;
    navigator.mediaSession.playbackState = "none";
  }
}
$("stop-player").onclick = stopPlayback;
window.addEventListener("pagehide", stopPlayback);
if (navigator.mediaSession) {
  for (const [action, handler] of Object.entries({
    play: () => video.play().catch(() => {}),
    pause: () => video.pause(),
    stop: stopPlayback,
    seekto: ({ seekTime }) =>
      selected && playRecorded(selected, timeline.target(seekTime)),
    seekbackward: ({ seekOffset }) => skip(-(seekOffset || 10)),
    seekforward: ({ seekOffset }) => skip(seekOffset || 30),
  })) {
    try {
      navigator.mediaSession.setActionHandler(action, handler);
    } catch (_) {}
  }
}
$("load-recorded").onclick = async () => {
  try {
    let items = await api("recorded");
    $("recorded").replaceChildren();
    for (let r of items) {
      let card = node("div", "");
      card.className = "card";
      card.append(
        node("strong", r.title),
        node("p", `${r.channel || ""} · ${r.date || ""} · ${r.duration || ""}`),
      );
      let b = node("button", "XA401から直接再生");
      b.onclick = () => {
        playRecorded(r);
        location.hash = "playback";
      };
      card.append(b);
      $("recorded").append(card);
    }
    message(items.length + "件の録画番組を取得しました。");
  } catch (e) {
    message(e.message);
  }
};
refresh();
setInterval(refresh, 5000);

// Preserve original-quality playback when entering KonomiTV's saved videos.
document.querySelector('a[href="/videos/"]').addEventListener("click", (e) => {
  try {
    const key = "KonomiTV-Settings";
    const settings = JSON.parse(localStorage.getItem(key) || "{}");
    if (!settings || typeof settings !== "object" || Array.isArray(settings))
      throw Error();
    Object.assign(settings, {
      video_streaming_quality: "original",
      video_streaming_quality_cellular: "original",
      video_data_saver_mode: false,
      video_data_saver_mode_cellular: false,
    });
    localStorage.setItem(key, JSON.stringify(settings));
  } catch (_) {
    e.preventDefault();
    message(
      "画質設定を保存できませんでした。KonomiTVの設定でOriginalを選んでください。",
    );
  }
});
