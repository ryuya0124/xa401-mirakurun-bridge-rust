export function durationSeconds(value) {
  if (typeof value !== "string" || !/^\d+:\d{2}:\d{2}(?:\.\d+)?$/.test(value))
    return 0;
  const [h, m, s] = value.split(":").map(Number);
  return m < 60 && s < 60 ? h * 3600 + m * 60 + s : 0;
}

export function clock(seconds) {
  seconds = Math.max(0, Math.floor(Number(seconds) || 0));
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = String(seconds % 60).padStart(2, "0");
  return h ? `${h}:${String(m).padStart(2, "0")}:${s}` : `${m}:${s}`;
}

export class RecordingTimeline {
  constructor(duration, offset = 0) {
    // The total comes from XA401's metadata, never the growing MSE buffer.
    this.duration = Math.max(0, Number(duration) || 0);
    this.offset = this.target(offset);
    this.started = false;
    this.base = 0;
  }
  target(seconds) {
    return Math.max(
      0,
      Math.min(Math.max(0, this.duration - 0.1), Number(seconds) || 0),
    );
  }
  start(mediaTime) {
    if (!this.started) {
      this.base = Number(mediaTime) || 0;
      this.started = true;
    }
  }
  position(mediaTime) {
    return Math.min(
      this.duration,
      this.offset + (this.started ? Math.max(0, mediaTime - this.base) : 0),
    );
  }
}
