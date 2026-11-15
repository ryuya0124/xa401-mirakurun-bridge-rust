import test from "node:test";
import assert from "node:assert/strict";
import { PlaybackSession } from "../assets/web/playback-session.mjs";

test("seeks reuse the player/worker; explicit stop destroys exactly once", async () => {
  let creates = 0,
    destroys = 0,
    urls = [];
  const session = new PlaybackSession(() => {
    creates++;
    return { load: async (url) => urls.push(url), destroy: () => destroys++ };
  });
  for (let p of [0, 600, 1790])
    assert.equal(await session.load(String(p)), true);
  assert.equal(creates, 1);
  assert.equal(destroys, 0);
  assert.deepEqual(urls, ["0", "600", "1790"]);
  session.destroy();
  session.destroy();
  assert.equal(destroys, 1);
});

test("a superseded rejection cannot report failure for a newer seek", async () => {
  const pending = [];
  const session = new PlaybackSession(() => ({
    load: () =>
      new Promise((resolve, reject) => pending.push({ resolve, reject })),
    destroy() {},
  }));
  const old = session.load("0");
  const latest = session.load("600");
  pending[0].reject(new Error("superseded"));
  pending[1].resolve();
  assert.equal(await old, false);
  assert.equal(await latest, true);
});
