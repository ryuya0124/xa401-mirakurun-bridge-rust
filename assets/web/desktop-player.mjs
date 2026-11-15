import { TimeSeekPlayer } from "./native-player.mjs?v=1";
import { Mpeg2TsPlayer } from "./player-lib/index.js";

// Public, JSON-only host protocol. No executable strings or Android services.
export class DesktopPlayback {
  constructor(video, report) {
    this.video = video;
    this.report = report;
    this.generation = 0;
    this.session = "";
    this.player = null;
    this.source = null;
    this.failed = false;
    this.notify = () => {
      if (!this.source) return;
      const total =
        this.source.kind === "recorded" ? this.source.duration : null;
      this.report({
        type: "state",
        session: this.session,
        position: Math.max(0, video.currentTime || 0),
        duration: total,
        paused: video.paused,
        ended: video.ended,
        ready: video.readyState >= 2,
        width: video.videoWidth,
        height: video.videoHeight,
        decodedFrames: video.getVideoPlaybackQuality?.().totalVideoFrames ?? 0,
        seeking: this.player?.state === "seeking",
        failed: this.failed,
      });
      if (navigator.mediaSession && total) {
        try {
          navigator.mediaSession.setPositionState({
            duration: total,
            position: Math.min(total, video.currentTime || 0),
            playbackRate: video.playbackRate,
          });
        } catch (_) {}
      }
    };
    this.interval = setInterval(this.notify, 250);
    for (const event of ["playing", "pause", "ended", "seeked"])
      video.addEventListener(event, this.notify);
    if (navigator.mediaSession)
      for (const [action, handler] of Object.entries({
        play: () => this.resume(),
        pause: () => video.pause(),
        seekto: (d) => this.seek(d.seekTime),
        seekforward: (d) => this.seek(video.currentTime + (d.seekOffset ?? 30)),
        seekbackward: (d) =>
          this.seek(video.currentTime - (d.seekOffset ?? 30)),
        stop: () => this.stop(),
      })) {
        try {
          navigator.mediaSession.setActionHandler(action, handler);
        } catch (_) {}
      }
  }
  async open(message) {
    const generation = ++this.generation;
    if (!/^[a-f0-9]{32}$/.test(message.session))
      throw Error("Invalid playback session");
    const source = message.source;
    const url = new URL(source.stream_url, location.origin);
    if (url.origin !== location.origin || url.search || url.hash)
      throw Error("Invalid stream origin");
    if (
      source.kind !== "recorded" &&
      (source.kind !== "live" ||
        !/^\/xa401\/api\/native\/channels\/\d+\/stream$/.test(url.pathname))
    )
      throw Error("Invalid live source");
    this.player?.destroy();
    this.session = message.session;
    this.source = source;
    this.failed = false;
    this.video.muted = message.muted === true;
    this.video.title = document.title = source.title;
    if (navigator.mediaSession && typeof MediaMetadata !== "undefined")
      navigator.mediaSession.metadata = new MediaMetadata({
        title: source.title,
        artist: "ReDiXiM",
        album: "XA401",
      });
    this.player =
      source.kind === "recorded"
        ? new TimeSeekPlayer(
            this.video,
            { pictureWorkers: message.pictureWorkers, audioGainSteps: 4 },
            source,
          )
        : new Mpeg2TsPlayer(this.video, {
            passthrough: false,
            audioGainSteps: 4,
          });
    this.player.addEventListener("error", () => {
      if (generation !== this.generation) return;
      this.failed = true;
      this.report({
        type: "error",
        session: this.session,
        message: "受信または再生に失敗しました。再生ボタンで再接続できます。",
      });
    });
    await this.player.load(String(url));
    if (generation !== this.generation) return;
    await this.video.play();
    this.notify();
  }
  seek(position) {
    if (
      !this.source ||
      this.source.kind !== "recorded" ||
      !Number.isFinite(position)
    )
      return;
    this.video.currentTime = Math.max(
      0,
      Math.min(this.source.duration - 0.1, position),
    );
  }
  async resume() {
    if (!this.player) return;
    if (this.failed) {
      this.failed = false;
      if (this.source.kind === "recorded")
        await this.player.startAt(this.video.currentTime);
      else
        await this.player.load(
          new URL(this.source.stream_url, location.origin),
        );
    }
    await this.video.play();
  }
  async command(message) {
    if (message?.type === "open") return this.open(message);
    if (!message || message.session !== this.session) return;
    switch (message.type) {
      case "toggle":
        return this.video.paused ? this.resume() : this.video.pause();
      case "play":
        return this.resume();
      case "pause":
        return this.video.pause();
      case "seek":
        return this.seek(message.position);
      case "stop":
        return this.stop();
    }
  }
  stop() {
    ++this.generation;
    this.player?.destroy();
    this.player = null;
    this.video.pause();
    this.source = null;
    if (navigator.mediaSession) {
      navigator.mediaSession.metadata = null;
      navigator.mediaSession.playbackState = "none";
    }
  }
  destroy() {
    this.stop();
    clearInterval(this.interval);
    for (const event of ["playing", "pause", "ended", "seeked"])
      this.video.removeEventListener(event, this.notify);
  }
}

const host = globalThis.chrome?.webview;
if (host) {
  const controller = new DesktopPlayback(
    document.querySelector("video"),
    (data) => host.postMessage(data),
  );
  host.addEventListener("message", (event) =>
    controller
      .command(event.data)
      .catch(() =>
        host.postMessage({
          type: "error",
          session: controller.session,
          message:
            "再生を開始できませんでした。Bridgeの接続を確認してください。",
        }),
      ),
  );
  window.addEventListener("pagehide", () => controller.destroy());
  host.postMessage({ type: "ready", protocol: 1 });
}
