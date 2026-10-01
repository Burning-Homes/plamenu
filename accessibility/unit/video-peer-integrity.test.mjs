import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { webcrypto, createHash } from 'node:crypto';
import { test } from 'node:test';
import assert from 'node:assert/strict';

// Run the shipped functions. Real RTC/HTTP decoder tests remain separate.
const source = readFileSync(new URL('../../crates/server/src/web/assets/app.js', import.meta.url), 'utf8');
const functions = source.slice(source.indexOf('  function peerSegmentValidator('), source.indexOf('  function loadHls('));
const data = Uint8Array.from([1, 2, 3, 4]);
const hash = createHash('sha256').update(data).digest('hex');
test('live no-byte deadlines adapt to segment length and remain finite', () => {
  const timeout = runInNewContext(`${functions}; liveSegmentTimeout`, {});
  assert.equal(timeout(2), 3000);
  assert.equal(timeout(4), 6000);
  assert.equal(timeout(100), 10000);
  assert.equal(timeout(Infinity), 3000);
});
test('returning live uses the current HLS target rather than a stale loaded tail', () => {
  const target = runInNewContext(`${functions}; liveSeekTarget`, {});
  const video = { seekable: { length: 1, end: () => 98 } };
  assert.equal(target(video, { liveSyncPosition: 100 }), 100);
  assert.equal(target(video, { liveSyncPosition: 95 }), 95);
  assert.equal(target(video, null), 98);
  assert.equal(target({ seekable: { length: 0 } }, null), null);
});
async function options(hashes, video = { dataset: { p2pUuid: 'video', p2pSwarm: 'swarm', p2pSha256: '/hashes', p2pTrackers: '["wss://tracker.test/socket"]' } }, live = false, hashLoader = async () => hashes) {
  return runInNewContext(`${functions}; peerOptions(video, live)`, {
    video,
    live, peerSharingEnabled: () => true, peerHashes: hashLoader,
    window: { RTCPeerConnection: {}, crypto: webcrypto }, crypto: webcrypto,
    navigator: {}, location: { href: 'https://video.test/' }, URL, Uint8Array, TextDecoder, atob, setTimeout,
  });
}
test('peer and HTTP completion share the shipped complete-segment integrity validator', async () => {
  const config = await options({ 'movie.mp4': { '10-13': hash } });
  assert.equal(config.core.validateHTTPSegment, config.core.validateP2PSegment);
  for (const validate of [config.core.validateP2PSegment, config.core.validateHTTPSegment]) {
    assert.equal(await validate('https://video.test/movie.mp4', { start: 10, end: 13 }, data), true);
    assert.equal(await validate('https://video.test/movie.mp4', { start: 10, end: 13 }, Uint8Array.from([1, 9, 3, 4])), false);
    assert.equal(await validate('https://video.test/movie.mp4', { start: 14, end: 17 }, data), false);
  }
});

test('cold live sharing retries an unpublished checksum manifest, but never retries VOD forever', async () => {
  let attempts = 0;
  const config = await options({}, undefined, true, async () => ++attempts === 1 ? null : {});
  assert.ok(config?.core);
  assert.equal(attempts, 2);
  attempts = 0;
  assert.equal(await options({}, undefined, false, async () => { attempts++; return null; }), null);
  assert.equal(attempts, 1);
});
test('native encoded proxy ranges are verified without treating mixed HTTP completion as trusted', async () => {
  const config = await options({ 'movie.mp4': { '10-13': hash } });
  const encoded = Buffer.from('https://origin.test/movie.mp4').toString('base64url');
  assert.equal(await config.core.validateHTTPSegment(`https://video.test/media/hls/1/seg?u=${encoded}&s=10&l=4`, undefined, data), true);
  assert.equal(await config.core.validateHTTPSegment(`https://video.test/media/hls/1/seg?u=${encoded}&s=10&l=4`, undefined, data.subarray(1)), false);
});

test('completed RTC checksum refusal is counted through the shipped library wrapped peer error', async () => {
  const video = { dataset: { p2pUuid: 'video', p2pSwarm: 'swarm', p2pSha256: '/hashes', p2pTrackers: '["wss://tracker.test/socket"]' } };
  const config = await options({}, video);
  const handlers = {};
  config.onHlsJsCreated({ p2pEngine: { addEventListener(name, callback) { handlers[name] = callback; } } });
  handlers.onSegmentError({ error: { type: 'peer-closed', cause: { type: 'validation-failed' } } });
  handlers.onSegmentError({ error: { type: 'p2p-segment-validation-failed' } });
  handlers.onSegmentError({ error: { type: 'peer-closed', cause: { type: 'connection-closed' } } });
  assert.equal(video.dataset.p2pValidationErrors, '2');
  handlers.onSegmentError({ error: { type: 'http-segment-validation-failed' } });
  assert.equal(video.dataset.hlsHttpValidationErrors, '1');
});

test('shipped peer storage has an exact finite byte budget and no late post-destroy publication', async () => {
  const config = await options({});
  const storage = config.core.customSegmentStorageFactory(false);
  await storage.initialize(config.core, { highDemandTimeWindow: 4 }, {});
  const updated = [];
  storage.setSegmentChangeCallback(stream => updated.push(stream));
  const part = new ArrayBuffer(24 * 1024 * 1024);
  for (let id = 0; id < 3; id++) await storage.storeSegment('swarm', 'video', id, part, id, id + 1, 'main', false);
  assert.equal(storage.hasSegment('swarm', 'video', 0), false);
  assert.equal(storage.hasSegment('swarm', 'video', 1), true);
  assert.equal(storage.getUsage().usedCapacity, 48);
  assert.equal(storage.getUsage().totalCapacity, 64);
  assert.equal((await storage.getSegmentData('swarm', 'video', 1)).byteLength, part.byteLength);
  storage.destroy();
  const before = updated.length;
  await storage.storeSegment('swarm', 'video', 99, part, 99, 100, 'main', false);
  storage.setSegmentChangeCallback(stream => updated.push(stream));
  storage.clear(true, 1);
  assert.equal(updated.length, before);
  assert.equal(storage.getUsage().usedCapacity, 0);
  assert.equal(storage.hasSegment('swarm', 'video', 99), false);
  assert.equal(await storage.getSegmentData('swarm', 'video', 1), undefined);
});

test('shipped live storage removes old generations without evicting the current playback window', async () => {
  const config = await options({});
  const storage = config.core.customSegmentStorageFactory(true);
  await storage.initialize(config.core, { highDemandTimeWindow: 4 }, {});
  storage.setSegmentChangeCallback(() => {});
  await storage.storeSegment('swarm', 'video', 0, new ArrayBuffer(10), 0, 2, 'main', true);
  await storage.storeSegment('swarm', 'video', 1, new ArrayBuffer(10), 8, 10, 'main', true);
  storage.onPlaybackUpdated(10, 1);
  storage.clear(true);
  assert.equal(storage.hasSegment('swarm', 'video', 0), false);
  assert.equal(storage.hasSegment('swarm', 'video', 1), true);
});
