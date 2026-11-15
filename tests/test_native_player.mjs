import test from 'node:test';
import assert from 'node:assert/strict';
import { validateSource, TimeSeekPlayer } from '../assets/web/native-player.mjs';

test('only the bounded same-origin recording stream is accepted', () => {
  const source = {stream_url:'/xa401/api/recorded/'+'a'.repeat(24)+'/stream', duration:1812.309};
  assert.equal(validateSource(source, 'https://tv.example').origin, 'https://tv.example');
  for (const bad of ['https://evil.example'+source.stream_url, '/api/services/1/stream', source.stream_url+'?url=http://evil.example'])
    assert.throws(() => validateSource({...source, stream_url:bad}, 'https://tv.example'));
  for (const bad of [0, NaN, Infinity, 86401])
    assert.throws(() => validateSource({...source, duration:bad}, 'https://tv.example'));
});

test('scrubbing back into the buffer cancels a queued network seek', async () => {
  const previousLocation = globalThis.location;
  globalThis.location = {origin:'https://tv.example'};
  const video = new EventTarget();
  Object.assign(video, {currentTime:0,readyState:0,videoWidth:0,
    buffered:{length:1,start:()=>0,end:()=>10}});
  const player = new TimeSeekPlayer(video,{}, {
    stream_url:'/xa401/api/recorded/'+'a'.repeat(24)+'/stream',duration:1812.309});
  const requested = [];
  player.state = 'converting'; player.buffer = {};
  player.startAt = async target => requested.push(target);
  try {
    video.currentTime=600; video.dispatchEvent(new Event('seeking'));
    video.currentTime=5; video.dispatchEvent(new Event('seeking'));
    await new Promise(resolve=>setTimeout(resolve,90));
    assert.deepEqual(requested,[]);
    video.currentTime=600; video.dispatchEvent(new Event('seeking'));
    video.currentTime=700; video.dispatchEvent(new Event('seeking'));
    await new Promise(resolve=>setTimeout(resolve,90));
    assert.deepEqual(requested,[700]);
  } finally { player.destroy(); globalThis.location=previousLocation; }
});
