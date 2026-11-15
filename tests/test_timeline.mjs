import test from "node:test";
import assert from "node:assert/strict";
import {
  durationSeconds,
  clock,
  RecordingTimeline,
} from "../assets/web/timeline.mjs";

test("full duration is fixed while the received buffer grows", () => {
  const timeline = new RecordingTimeline(durationSeconds("00:30:12.309"));
  timeline.start(0);
  assert.equal(timeline.position(30), 30);
  assert.equal(timeline.position(120), 120);
  assert.equal(timeline.duration, 1812.309);
  assert.equal(timeline.target(1700), 1700);
});
test("seek position includes the offset and resets the media baseline once", () => {
  const timeline = new RecordingTimeline(1800, 600);
  assert.equal(timeline.position(0), 600);
  timeline.start(17);
  assert.equal(timeline.position(22), 605);
  timeline.start(99); // A later playing event after pause must not reset elapsed time.
  assert.equal(timeline.position(27), 610);
  assert.equal(timeline.target(9999), 1799.9);
  assert.equal(timeline.target(-10), 0);
});
test("duration parsing and visible clock do not use MSE duration", () => {
  assert.equal(durationSeconds(null), 0);
  assert.equal(durationSeconds("bad"), 0);
  assert.equal(durationSeconds("00:99:00"), 0);
  assert.equal(clock(1812.309), "30:12");
  assert.equal(clock(3723), "1:02:03");
});
