// Keep the worker and video element alive across DLNA time seeks.
export class PlaybackSession {
  constructor(create) {
    this.create = create;
    this.player = null;
    this.generation = 0;
  }
  async load(url) {
    const generation = ++this.generation;
    this.player ??= this.create();
    try {
      await this.player.load(url);
      return generation === this.generation;
    } catch (error) {
      if (generation !== this.generation) return false;
      throw error;
    }
  }
  destroy() {
    ++this.generation;
    this.player?.destroy();
    this.player = null;
  }
}
