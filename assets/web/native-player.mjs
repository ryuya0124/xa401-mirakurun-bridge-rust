// DLNA NPT source adapter for KonomiTV's existing DPlayer and Media Session.
// Uses the unmodified MIT mpeg2toh264 worker; see player-lib/LICENSE + SOURCE.json.
const WORKER = "/xa401/player-lib/assets/worker-CdUcgpWr.js";
const inside = (ranges, time) => {
  for (let i = 0; i < ranges.length; ++i)
    if (ranges.start(i) - 0.05 <= time && time <= ranges.end(i) - 0.05)
      return true;
  return false;
};

export function validateSource(source, origin = location.origin) {
  const url = new URL(source.stream_url, origin);
  if (
    url.origin !== origin ||
    !/^\/xa401\/api\/recorded\/[a-f0-9]{24}\/stream$/.test(url.pathname) ||
    url.search ||
    url.hash ||
    !Number.isFinite(source.duration) ||
    source.duration <= 0 ||
    source.duration > 86400
  )
    throw new Error("Invalid external recording source");
  return url;
}

export function createPlayerClass(_OriginalPlayer, source) {
  validateSource(source);
  return class extends TimeSeekPlayer {
    constructor(video, options = {}) {
      super(video, options, source);
    }
  };
}

export class TimeSeekPlayer extends EventTarget {
  constructor(video, options, source) {
    super();
    this.video = video;
    this.options = options;
    this.source = source;
    this.baseURL = validateSource(source);
    this.duration = source.duration;
    this.playerId = crypto.randomUUID().replaceAll("-", "");
    this.id = 0;
    this.state = "idle";
    this.audio = null;
    this.mediaSourceOwner = "main";
    this.queue = [];
    this.offset = 0;
    this.buffer = null;
    this.media = null;
    this.worker = null;
    this.destroyed = false;
    this.pending = null;
    this.operation = null;
    this.clearNeeded = false;
    this.firstFragment = true;
    this.wantFinish = false;
    this.videoTimeline = [];
    this.deinterlacer = null;
    this.deinterlace = options.deinterlace ?? false;
    this.onSeeking = () => {
      if (this.destroyed || this.state === "idle" || !this.buffer) return;
      const target = this.video.currentTime;
      clearTimeout(this.seekTimer);
      if (inside(this.video.buffered, target)) return;
      // Coalesce a scrub gesture, retaining the same video/MS/worker objects.
      this.seekTimer = setTimeout(
        () => this.startAt(target).catch(() => {}),
        60,
      );
    };
    video.addEventListener("seeking", this.onSeeking);
    this.tick = setInterval(() => {
      this.pump();
      this.flow();
    }, 150);
  }
  emit(type, detail) {
    this.dispatchEvent(new CustomEvent(type, { detail }));
  }
  setState(state) {
    this.state = state;
    this.emit("statechange", { state });
  }
  get deinterlace() {
    return this.deinterlacer?.running ?? false;
  }
  get deinterlaceWanted() {
    return this.wanted;
  }
  set deinterlace(enabled) {
    this.wanted = enabled;
    if (enabled && !this.deinterlacer && this.options.deinterlacer) {
      this.deinterlacer = this.options.deinterlacer(this.video);
      this.deinterlacer.videoTimeline = this.videoTimeline;
    }
    if (this.deinterlacer) this.deinterlacer.enabled = enabled;
  }
  async load() {
    if (this.destroyed) throw new Error("Player was destroyed");
    if (!this.media) {
      const Media = globalThis.MediaSource || globalThis.ManagedMediaSource;
      if (!Media) throw new Error("Media Source Extensions are unavailable");
      this.media = new Media();
      this.objectURL = URL.createObjectURL(this.media);
      const opened = new Promise((resolve) =>
        this.media.addEventListener("sourceopen", resolve, { once: true }),
      );
      if (Media === globalThis.ManagedMediaSource) {
        this.oldRemotePlayback = this.video.disableRemotePlayback;
        this.video.disableRemotePlayback = true;
        this.childSource = document.createElement("source");
        this.childSource.type = "video/mp4";
        this.childSource.src = this.objectURL;
        this.video.removeAttribute("src");
        this.video.append(this.childSource);
        this.video.load();
      } else this.video.src = this.objectURL;
      await Promise.race([
        opened,
        new Promise((_, reject) => {
          this.openTimer = setTimeout(
            () => reject(new Error("Media source open timed out")),
            15000,
          );
        }),
      ]);
      clearTimeout(this.openTimer);
      this.media.duration = this.duration;
      this.worker = new Worker(WORKER, { type: "module" });
      this.worker.onmessage = (event) => this.message(event.data);
      this.worker.onerror = () => this.fail(new Error("Video worker failed"));
    }
    return this.startAt(0);
  }
  holdFrame() {
    if (this.frameCallback != null)
      this.video.cancelVideoFrameCallback?.(this.frameCallback);
    if (!this.video.videoWidth || this.video.readyState < 2) return;
    this.overlay ??= document.createElement("canvas");
    const canvas = this.overlay;
    canvas.setAttribute("aria-hidden", "true");
    Object.assign(canvas.style, {
      position: "absolute",
      inset: "0",
      width: "100%",
      height: "100%",
      objectFit: "contain",
      pointerEvents: "none",
      zIndex: "2",
    });
    canvas.width = Math.min(1920, this.video.videoWidth);
    canvas.height = Math.round(
      (canvas.width * this.video.videoHeight) / this.video.videoWidth,
    );
    try {
      canvas
        .getContext("2d")
        .drawImage(this.video, 0, 0, canvas.width, canvas.height);
      this.video.parentElement.append(canvas);
      canvas.hidden = false;
    } catch (_) {
      canvas.hidden = true;
    }
  }
  releaseFrame() {
    const id = this.id;
    const done = () => {
      if (id === this.id && this.overlay) this.overlay.hidden = true;
    };
    if (this.video.requestVideoFrameCallback)
      this.frameCallback = this.video.requestVideoFrameCallback(done);
    else requestAnimationFrame(() => requestAnimationFrame(done));
  }
  startAt(position) {
    if (this.destroyed || !this.worker)
      return Promise.reject(new Error("Player is unavailable"));
    this.holdFrame();
    this.pending?.reject(new Error("Load superseded"));
    clearTimeout(this.loadTimer);
    this.worker.postMessage({ type: "stop", id: this.id });
    ++this.id;
    this.offset = Math.max(
      0,
      Math.min(this.duration - 0.1, Number(position) || 0),
    );
    this.queue = [];
    this.clearNeeded = !!this.buffer;
    this.firstFragment = true;
    this.wantFinish = false;
    this.videoTimeline = [];
    if (this.deinterlacer) this.deinterlacer.videoTimeline = [];
    const url = new URL(this.baseURL);
    url.searchParams.set("player", this.playerId);
    url.searchParams.set("position", this.offset.toFixed(3));
    const pending = new Promise((resolve, reject) => {
      this.pending = { resolve, reject };
    });
    // Internal seeks have no caller to await; still emit their errors below.
    pending.catch(() => {});
    this.loadTimer = setTimeout(
      () => this.fail(new Error("XA401 playback timed out")),
      45000,
    );
    this.setState(position ? "seeking" : "loading");
    this.worker.postMessage({
      type: "load",
      id: this.id,
      url: String(url),
      wasmUrl: null,
      sink: "main",
      encodedVideo: false,
      passthrough: false,
      preferManagedMediaSource: false,
      serviceId: null,
      oversample: this.options.oversample,
      recoveryInterval: this.options.recoveryInterval,
      openGopRecovery: this.options.openGopRecovery,
      splitFieldSamples: this.options.splitFieldSamples,
      audioGainSteps: this.options.audioGainSteps ?? 0,
      surroundAudioGainSteps:
        this.options.surroundAudioGainSteps ?? this.options.audioGainSteps ?? 0,
      pictureWorkers: this.options.pictureWorkers,
      queueHighWaterMark: 33554432,
      maxAheadSeconds: 8,
      keepBehindSeconds: 20,
    });
    if (this.selectedAudio != null || this.selectedSub != null)
      this.worker.postMessage({
        type: "audio",
        id: this.id,
        pid: this.selectedAudio ?? null,
        dualMonoSub: this.selectedSub ?? null,
      });
    this.pump();
    return pending;
  }
  message(message) {
    if (this.destroyed || message.id !== this.id) return;
    try {
      switch (message.type) {
        case "open":
          if (!this.buffer) {
            if (!this.media.constructor.isTypeSupported(message.mimeCodec))
              throw new Error("Unsupported video codec");
            this.buffer = this.media.addSourceBuffer(message.mimeCodec);
            this.codec = message.mimeCodec;
            this.buffer.mode = "segments";
            this.buffer.addEventListener("updateend", () => this.updated());
            this.buffer.addEventListener("error", () =>
              this.fail(new Error("Invalid media fragment")),
            );
          }
          this.queue.push({
            data: message.data,
            init: true,
            codec: message.mimeCodec,
            id: this.id,
          });
          this.flow();
          this.pump();
          break;
        case "fragment":
          this.queue.push({
            data: message.data,
            init: false,
            start: message.start + this.offset,
            id: this.id,
          });
          this.flow();
          this.pump();
          break;
        case "video-config":
          this.videoTimeline.push({
            start: message.start + this.offset,
            codedSize: { width: message.width, height: message.height },
          });
          if (this.deinterlacer)
            this.deinterlacer.videoTimeline = this.videoTimeline;
          break;
        case "scans":
          for (const scan of message.scans)
            this.videoTimeline.push({ start: scan.start + this.offset, scan });
          this.videoTimeline.sort((a, b) => a.start - b.start);
          if (this.deinterlacer)
            this.deinterlacer.videoTimeline = this.videoTimeline;
          break;
        case "audio":
          this.audio = message.audio;
          this.emit("audio", message.audio);
          break;
        case "services":
          this.emit("services", message.services);
          break;
        case "stats":
          this.emit("stats", message.stats);
          break;
        case "progress":
          this.emit("progress", {
            bytesRead: message.bytesRead,
            totalBytes: message.totalBytes,
          });
          break;
        case "workers":
          this.emit("workers", { pictureWorkers: message.pictureWorkers });
          break;
        case "private_stream_1":
        case "private_stream_2":
          this.emit(message.type, {
            ...message.stream,
            pts:
              message.stream.pts == null
                ? null
                : message.stream.pts + this.offset,
          });
          break;
        case "finish":
          this.wantFinish = true;
          this.pump();
          break;
        case "error":
          this.fail(new Error(message.message));
          break;
      }
    } catch (error) {
      this.fail(error);
    }
  }
  pump() {
    const buffer = this.buffer;
    if (
      !buffer ||
      buffer.updating ||
      this.operation ||
      this.destroyed ||
      this.state === "error"
    )
      return;
    try {
      if (this.clearNeeded) {
        this.clearNeeded = false;
        this.operation = { clear: true };
        buffer.remove(0, Infinity);
        return;
      }
      // Bound memory even during long playback. Retain a generous GOP margin.
      if (
        !this.firstFragment &&
        buffer.buffered.length &&
        buffer.buffered.start(0) < this.video.currentTime - 30
      ) {
        this.operation = { evict: true };
        buffer.remove(0, this.video.currentTime - 20);
        return;
      }
      const item = this.queue[0];
      if (!item) {
        if (this.wantFinish && this.media.readyState === "open") {
          this.wantFinish = false;
          this.media.endOfStream();
          this.setState("completed");
        }
        return;
      }
      if (item.init) {
        if (this.codec !== item.codec) {
          buffer.changeType(item.codec);
          this.codec = item.codec;
        }
        buffer.timestampOffset = this.offset;
        buffer.appendWindowEnd = this.duration;
        if (this.media.duration !== this.duration)
          this.media.duration = this.duration;
      }
      this.operation = item;
      buffer.appendBuffer(item.data);
    } catch (error) {
      this.operation = null;
      this.fail(error);
    }
  }
  updated() {
    const operation = this.operation;
    this.operation = null;
    if (operation?.id === this.id && this.queue[0] === operation) {
      this.queue.shift();
      if (
        !operation.init &&
        this.firstFragment &&
        this.buffer.buffered.length
      ) {
        this.firstFragment = false;
        // First available A/V sample can be slightly after the requested NPT.
        const start = this.buffer.buffered.start(0);
        if (!inside(this.video.buffered, this.video.currentTime))
          this.video.currentTime = Math.max(this.offset, start);
        clearTimeout(this.loadTimer);
        this.pending?.resolve();
        this.pending = null;
        this.setState("converting");
        this.emit("seekable", { duration: this.duration });
        this.releaseFrame();
      }
    }
    this.flow();
    this.pump();
  }
  flow() {
    if (!this.worker || !this.buffer || this.destroyed) return;
    const buffered = this.buffer.buffered;
    const ahead = buffered.length
      ? buffered.end(buffered.length - 1) -
        Math.max(this.offset, this.video.currentTime)
      : 0;
    const bytes = this.queue.reduce(
      (sum, item) => sum + item.data.byteLength,
      0,
    );
    if (bytes > 33554432) {
      this.fail(new Error("Video append queue limit exceeded"));
      return;
    }
    const ready =
      !this.clearNeeded &&
      this.queue.length < 2 &&
      ahead < 8 &&
      this.state !== "error";
    this.worker.postMessage({ type: "flow", id: this.id, ready });
  }
  selectAudio(pid) {
    this.selectedAudio = pid;
    this.worker?.postMessage({
      type: "audio",
      id: this.id,
      pid,
      dualMonoSub: null,
    });
  }
  selectDualMono(sub) {
    this.selectedSub = sub;
    this.worker?.postMessage({
      type: "audio",
      id: this.id,
      pid: null,
      dualMonoSub: sub,
    });
  }
  fail(error) {
    if (this.state === "error" || this.destroyed) return;
    clearTimeout(this.loadTimer);
    this.worker?.postMessage({ type: "stop", id: this.id });
    this.pending?.reject(error);
    this.pending = null;
    this.setState("error");
    this.emit("error", { error });
  }
  stop() {
    clearTimeout(this.seekTimer);
    clearTimeout(this.loadTimer);
    this.worker?.postMessage({ type: "stop", id: this.id });
    ++this.id;
    this.pending?.reject(new Error("Playback stopped"));
    this.pending = null;
    this.queue = [];
    this.setState("idle");
  }
  destroy() {
    if (this.destroyed) return;
    this.stop();
    this.destroyed = true;
    clearInterval(this.tick);
    clearTimeout(this.openTimer);
    this.video.removeEventListener("seeking", this.onSeeking);
    if (this.frameCallback != null)
      this.video.cancelVideoFrameCallback?.(this.frameCallback);
    this.overlay?.remove();
    this.deinterlacer?.destroy();
    this.worker?.terminate();
    this.childSource?.remove();
    if (this.oldRemotePlayback !== undefined)
      this.video.disableRemotePlayback = this.oldRemotePlayback;
    if (this.objectURL && this.video.src === this.objectURL)
      this.video.removeAttribute("src");
    if (this.objectURL) URL.revokeObjectURL(this.objectURL);
  }
}
