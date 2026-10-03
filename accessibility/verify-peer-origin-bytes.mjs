// Matched native HTTP-emission measurement, including seed cost. For a
// PeerTube source this measures the Plamenu consumer, NOT PeerTube upstream.
import { chromium } from 'playwright';
import {selectVideoQuality} from './video-settings.mjs';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
const [manifestPath, control, evidence] = process.argv.slice(2);
const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
const browser = await chromium.launch();
const contexts = [];
const budgets = { viewers: 8, startupMs: 30000, steadySeconds: 20,
  minimumFramesPerSecond: 10, minimumMediaSecondsPerSecond: .5,
  minimumSharingViewers: 4, minimumTotalBodySavingsFraction: .05 };
const result = { status: 'running', source: manifest.origin, budgets, arms: [],
  scope: 'actual Plamenu HTTP /media/ response-body emissions; seed included; headers, framing and tracker bytes excluded; not PeerTube upstream byte accounting',
  network: 'owned real TCP bridge: same configured HTTP latency/bandwidth in both arms; no browser media response stubs' };
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
function save() { writeFileSync(join(evidence, 'peer-origin-bytes-browser.json'), JSON.stringify(result, null, 2), { mode: 0o600 }); }
async function metrics(action = 'status') {
  const response = await fetch(`${control}/${action}`, { method: 'POST', headers: { 'X-AP09-Control': process.env.AP09_PROXY_TOKEN } });
  if (!response.ok) throw new Error('owned bridge control failed');
  return response.json();
}
async function sample(video) {
  return video.evaluate(v => ({ time: v.currentTime, error: v.error?.code || null, height: v.videoHeight,
    presented: v.getVideoPlaybackQuality().totalVideoFrames - v.getVideoPlaybackQuality().droppedVideoFrames,
    dropped: v.getVideoPlaybackQuality().droppedVideoFrames,
    validated: Number(v.dataset.p2pValidated || 0), uploaded: Number(v.dataset.p2pUploaded || 0),
    peerErrors: Number(v.dataset.p2pValidationErrors || 0), httpErrors: Number(v.dataset.hlsHttpValidationErrors || 0),
    buffers: [...Array(v.buffered.length)].map((_, n) => [v.buffered.start(n), v.buffered.end(n)]) }));
}
async function until(predicate, deadlineMs, description) {
  const end = Date.now() + deadlineMs;
  while (Date.now() < end) { if (await predicate()) return; await pause(200); }
  throw new Error(description);
}
async function open(sharing) {
  const context = await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 320, height: 800 } });
  await context.addInitScript(()=>{window.__ownedVideoProfile=true;});
  contexts.push(context);
  // Explicit browser opt-out is set BEFORE app initialization/tracker contact.
  if (!sharing) await context.addInitScript(() => localStorage.setItem('plamenu:p2p-enabled', 'false'));
  const page = await context.newPage(), errors = [], sockets = [];
  page.on('pageerror', error => errors.push(String(error)));
  page.on('websocket', socket => sockets.push(new URL(socket.url()).pathname));
  const began = Date.now();
  await page.goto(manifest.local, { waitUntil: 'domcontentloaded' });
  const video = page.locator('video[data-hls]').first();
  await video.evaluate(v => { v.muted = true; v.play().catch(() => {}); });
  try {
    await selectVideoQuality(page, '240p');
  } catch(error) {
    result.missingQualitySnapshot=await video.evaluate(v=>({sampledAtMs: performance.now(), playbackRate: v.playbackRate, seeking: v.seeking,
    bufferedRanges: [...Array(v.buffered.length)].map((_, n) => [v.buffered.start(n), v.buffered.end(n)]),
    seekableRanges: [...Array(v.seekable.length)].map((_, n) => [v.seekable.start(n), v.seekable.end(n)]),
    visibilityClass: document.visibilityState === 'visible' ? 'visible' : 'hidden',
    sourceClass: !v.currentSrc ? 'empty' : v.currentSrc.startsWith('blob:') ? 'blob-media-source' : 'resource-source',
    profile: window.__ownedVideoProfiles?.get(v)?.() || null,
    time:v.currentTime,ready:v.readyState,error:v.error?.code||null,
      presented:v.getVideoPlaybackQuality().totalVideoFrames-v.getVideoPlaybackQuality().droppedVideoFrames}));
    save();
    throw error;
  }
  await until(async () => { const s = await sample(video); return s.height === 240 && s.presented >= 8 && s.time > .2 && !s.error; }, budgets.startupMs, 'matched viewer did not decode 240p');
  if (!sharing && sockets.length) throw new Error('pre-contact opt-out contacted tracker');
  return { context, page, video, errors, sockets, startupMs: Date.now() - began };
}
try {
  await metrics('constrain');
  // Reverse order for the other source. Neither arm retains browser peers.
  for (const sharing of manifest.origin === 'native' ? [false, true] : [true, false]) {
    const before = await metrics();
    const seed = await open(sharing);
    await until(async () => (await sample(seed.video)).time >= 20, 45000, 'seed did not decode 20 seconds');
    await seed.video.evaluate(v => v.pause());
    const afterSeed = await metrics();
    const clients = await Promise.all([...Array(budgets.viewers)].map(() => open(sharing)));
    await pause(3000);
    const initial = await Promise.all(clients.map(client => sample(client.video)));
    await pause(budgets.steadySeconds * 1000);
    const final = await Promise.all(clients.map(client => sample(client.video)));
    const after = await metrics();
    const arm = { sharing, seed: await sample(seed.video),
      seedBodyBytes: afterSeed.origin_media_body_bytes - before.origin_media_body_bytes,
      viewerBodyBytes: after.origin_media_body_bytes - afterSeed.origin_media_body_bytes,
      totalBodyBytes: after.origin_media_body_bytes - before.origin_media_body_bytes,
      responseCount: after.origin_media_responses - before.origin_media_responses,
      clients: clients.map((client, n) => ({ startupMs: client.startupMs, initial: initial[n], final: final[n], pageErrors: client.errors })) };
    result.arms.push(arm); save();
    for (const [n, client] of clients.entries()) {
      if (client.errors.length || final[n].error || final[n].height !== 240 ||
          final[n].presented - initial[n].presented < budgets.steadySeconds * budgets.minimumFramesPerSecond ||
          final[n].time - initial[n].time < budgets.steadySeconds * budgets.minimumMediaSecondsPerSecond)
        throw new Error(`matched arm decoder budget failed: ${sharing}/${n}`);
      if (!sharing && (final[n].validated || client.sockets.length)) throw new Error('server-only arm used peers');
      await client.context.close();
    }
    await seed.context.close();
  }
  const off = result.arms.find(arm => !arm.sharing), on = result.arms.find(arm => arm.sharing);
  result.totalBodySavingsFraction = 1 - on.totalBodyBytes / off.totalBodyBytes;
  result.sharingViewers = on.clients.filter(client => client.final.validated > 0).length;
  if (off.totalBodyBytes <= 0 || on.seedBodyBytes <= 0 || result.sharingViewers < budgets.minimumSharingViewers ||
      result.totalBodySavingsFraction < budgets.minimumTotalBodySavingsFraction)
    throw new Error('measured seed-inclusive origin-byte savings did not meet declared workload budget');
  result.status = 'passed'; save();
} catch (error) { result.status = 'failed'; result.error = String(error); save(); throw error; }
finally { await metrics('unconstrain').catch(() => {}); for (const context of contexts) await context.close().catch(() => {}); await browser.close(); }
