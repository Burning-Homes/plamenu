// Eight real decoders / two broadcasts, then actual tracker TCP loss via an
// owned bridge. Media and WebRTC stay real; only warm-up HTTP delay is injected
// explicitly to establish peer exchange before the undelayed steady phase.
import { chromium } from 'playwright';
import { installPeerTubeWelcomeHandler } from './peertube-session.mjs';
import {openVideoSettings,closeVideoSettings,selectVideoQuality} from './video-settings.mjs';
import {observeLiveSegments} from './live-segment-observer.mjs';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
const [manifestPath, controlUrl, evidence] = process.argv.slice(2);
const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
const browser = await chromium.launch();
const clients = [];
const pages = [];
const progress = { phases: [], status: 'running' };
let observingSteady=false;
const wireObservers=[];
let peerAuth;
const publicUrl = value => { const url = new URL(value); return `${url.origin}${url.pathname.startsWith('/plugins/') ? '/plugins/[redacted]' : url.pathname}`; };
function checkpoint(stage) {
  progress.stage = stage;
  progress.at = Date.now();
  writeFileSync(join(evidence, 'concurrent-live-progress.json'), JSON.stringify(progress, null, 2), { mode: 0o600 });
}
// Native filenames, actual PeerTube filenames and canonical remote-live
// proxy aliases all retain the MPEG-TS suffix.
const segment = /\.ts(?:\?|$)/;
const pause = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));
async function control(action) {
  const response = await fetch(`${controlUrl}/${action}`, { method: 'POST', headers: { 'X-AP09-Control': process.env.AP09_PROXY_TOKEN } });
  if (!response.ok) throw new Error(`owned bridge control failed ${response.status}`);
  return response.json();
}
async function sample(client) {
  return client.video.evaluate((v, clock) => {
    let latencyMs = null;
    if (clock && v.readyState >= 2) {
      const canvas = document.createElement('canvas'); canvas.width = 320; canvas.height = 180;
      const context = canvas.getContext('2d', { willReadFrequently: true });
      context.drawImage(v, 0, 0, 320, 180);
      const pixels = context.getImageData(0, 6, 320, 1).data;
      let ticks = 0;
      for (let bit = 0; bit < 32; bit++) { const at = (bit * 10 + 5) * 4; if (pixels[at] + pixels[at + 1] + pixels[at + 2] > 384) ticks += 2 ** bit; }
      const now = Math.floor(Date.now() / 100) % (2 ** 32);
      const elapsed = (now - ticks + 2 ** 32) % (2 ** 32);
      if (elapsed <= 300) latencyMs = elapsed * 100;
    }
    return { sampledAtMs: performance.now(), playbackRate: v.playbackRate, seeking: v.seeking,
    bufferedRanges: [...Array(v.buffered.length)].map((_, n) => [v.buffered.start(n), v.buffered.end(n)]),
    seekableRanges: [...Array(v.seekable.length)].map((_, n) => [v.seekable.start(n), v.seekable.end(n)]),
    visibilityClass: document.visibilityState === 'visible' ? 'visible' : 'hidden',
    sourceClass: !v.currentSrc ? 'empty' : v.currentSrc.startsWith('blob:') ? 'blob-media-source' : 'resource-source',
    profile: window.__ownedVideoProfiles?.get(v)?.() || null,
    time: v.currentTime, paused: v.paused, ready: v.readyState,
      frames: v.getVideoPlaybackQuality().totalVideoFrames, dropped: v.getVideoPlaybackQuality().droppedVideoFrames,
      error: v.error?.code || null, validated: Number(v.dataset.p2pValidated || 0), latencyMs,
      downloaded: Number(v.dataset.p2pDownloaded || 0), infohash: v.dataset.p2pInfohash || null, lastPeerError: v.dataset.p2pLastError || null,
      integrityFailure: v.dataset.p2pIntegrityFailure || null,
      uploaded: Number(v.dataset.p2pUploaded || 0), integrityErrors: Number(v.dataset.p2pValidationErrors || 0) + Number(v.dataset.hlsHttpValidationErrors || 0) };
  }, client.kind === 'native');
}
async function waitDecoded(client, timeout = 25000) {
  const began = Date.now();
  while (Date.now() - began < timeout) {
    const state = await sample(client);
    if (!state.paused && state.frames >= 8 && state.time > 0.2 && !state.error) return state;
    await pause(100);
  }
  throw new Error(`native decoder stalled (${client.kind}): ${JSON.stringify(await sample(client))}`);
}
async function open(kind, url, delayMs, broadcast) {
  const setting=manifest[broadcast];
  const context = await browser.newContext({ ignoreHTTPSErrors: true,
    ...(kind === 'peertube' && peerAuth ? { storageState: peerAuth } : {}),
    viewport: { width: kind === 'native' ? 320 : 800, height: 800 } });
  await context.addInitScript(()=>{window.__ownedVideoProfile=true;});
  if(setting.trackerVia)await context.addInitScript(({original,via})=>{
    const NativeSocket=WebSocket;
    window.__ownedTrackerMappings=0;
    window.WebSocket=class extends NativeSocket {
      constructor(url,...args){
        const value=new URL(String(url),location.href),source=new URL(original);
        if(value.origin===source.origin&&value.pathname==='/tracker/socket'){
          const mapped=new URL(via);mapped.search=value.search;url=mapped.href;window.__ownedTrackerMappings++;
        }
        super(url,...args);
      }
    };
  },{original:setting.trackerOrigin,via:setting.trackerVia});
  const page = await context.newPage();
  if(process.env.AP09_CONCURRENCY_TRACE==='1'&&kind==='native'&&delayMs===0){
    wireObservers.push(observeLiveSegments(page,evidence,broadcast,()=>observingSteady));
  }
  if (kind === 'peertube') await installPeerTubeWelcomeHandler(page);
  const diagnostic = { kind, broadcast, page, errors: [], failedRequests: [] };
  pages.push(diagnostic);
  page.on('pageerror', error => diagnostic.errors.push(String(error)));
  page.on('response', response => { if (response.status() >= 400) diagnostic.errors.push(`${response.status()} ${publicUrl(response.url())}`); });
  page.on('requestfailed', request => {
    if(diagnostic.failedRequests.length < 256) diagnostic.failedRequests.push({url:publicUrl(request.url()),reason:request.failure()?.errorText});
  });
  const sockets = [];
  page.on('websocket', ws => { const record = { url: publicUrl(ws.url()), opened: Date.now(), closed: null }; sockets.push(record); ws.on('close', () => { record.closed = Date.now(); }); });
  if (kind === 'peertube' && !peerAuth) {
    await page.goto(new URL('/login', url).href);
    await page.locator('#username').fill(process.env.PEERTUBE_E2E_USER);
    await page.locator('#password').fill(process.env.PEERTUBE_E2E_PASSWORD);
    await page.locator('input[type="submit"][value="Login"]').click();
    await page.waitForURL(value => value.pathname !== '/login');
    // One real login, independent browser contexts thereafter. Authentication
    // is not this workload; avoid unrelated login rate limits/redirect races.
    // This credential-bearing state stays exclusively in process memory.
    peerAuth = await context.storageState();
  }
  await page.goto(url, { waitUntil: 'domcontentloaded' });
  if (kind === 'peertube') {
    const consent = page.getByRole('button', { name: 'OK', exact: true });
    await consent.waitFor({ state: 'visible', timeout: 3000 }).catch(() => {});
    if (await consent.isVisible()) await consent.click();
  } else {
    await openVideoSettings(page);
    const choice = page.locator('.media__p2p-toggle');
    await choice.waitFor({ state: 'visible' });
    if (!await choice.isChecked()) throw new Error('live sharing did not default on');
    if (!await page.getByText(/network address/).isVisible()) throw new Error('sharing disclosure missing');
    await closeVideoSettings(page);
  }
  const video = page.locator(kind === 'native' ? 'video[data-hls]' : 'video.vjs-tech');
  await video.waitFor({ state: 'attached' });
  if (kind === 'peertube') {
    const play = page.locator('.vjs-big-play-button');
    if (await play.isVisible()) await play.click();
  }
  await video.evaluate(v => { v.muted = true; v.play().catch(() => {}); });
  if(setting.qualityHeight){
    if(setting.qualitySelectable === false){
      // A single source rendition has no artificial quality menu. Require
      // the actual decoded height below, just as for a selected VOD rung.
    } else if(kind==='native'){
      await selectVideoQuality(page, `${setting.qualityHeight}p`);
    }else{
      await video.hover({force:true});await page.locator('.vjs-settings').click();
      await page.locator('.vjs-main-menu .vjs-menu-item').filter({hasText:'Quality'}).click();
      const menu=page.locator('.vjs-menu').filter({has:page.locator('.vjs-back-button').filter({hasText:'Quality'})});
      await menu.locator('[role="menuitemradio"]',{hasText:`${setting.qualityHeight}p`}).click();
    }
    await page.waitForFunction(({selector,height})=>document.querySelector(selector)?.videoHeight===height,
      {selector:kind==='native'?'video[data-hls]':'video.vjs-tech',height:setting.qualityHeight},{timeout:15000});
  }
  const client = { kind, broadcast, context, page, video, sockets };
  clients.push(client);
  await waitDecoded(client);
  if(setting.trackerVia&&!(await page.evaluate(()=>window.__ownedTrackerMappings)))throw new Error('owned endpoint mapping did not reach real PeerTube tracker');
  checkpoint(`decoded ${kind} broadcast ${broadcast}; ${clients.length} clients`);
  if (delayMs) await page.route(segment, async route => {
    await pause(delayMs);
    await route.continue().catch(error => {
      // A player's warm-up timeout can cancel a delayed request. Record only
      // this expected routing lifecycle case, never swallow unrelated faults.
      if (!/Route is already handled|Target page, context or browser has been closed/.test(String(error))) throw error;
      diagnostic.errors.push('warm-up delayed request cancelled by player');
    });
  });
  return client;
}
async function phase(name, seconds) {
  checkpoint(`starting ${name}`);
  const baseline = await Promise.all(clients.map(sample));
  const observations = [];
  const until = Date.now() + seconds * 1000;
  while (Date.now() < until) { observations.push(await Promise.all(clients.map(sample))); await pause(500); }
  const final = await Promise.all(clients.map(sample));
  const deltas = final.map((state, n) => ({ kind: clients[n].kind, broadcast: clients[n].broadcast,
    frames: state.frames - baseline[n].frames, presented: state.frames - baseline[n].frames - state.dropped + baseline[n].dropped, mediaSeconds: state.time - baseline[n].time,
    dropped: state.dropped - baseline[n].dropped, error: state.error, validated: state.validated,
    uploaded: state.uploaded, integrityErrors: state.integrityErrors }));
  // Preserve the complete numeric phase even when its unchanged gate fails.
  progress.lastMeasuredPhase = { name, seconds, baseline, final, deltas, observations };
  checkpoint(`measured ${name}`);
  if (deltas.some(state => state.presented < seconds * 10 || state.mediaSeconds < seconds / 2 || state.error || state.integrityErrors)) {
    throw new Error(`concurrent ${name} decode failed: ${JSON.stringify(deltas)}`);
  }
  // sample() deliberately reads the pixel clock only in native players.
  const latency = observations.flatMap(states => states.map(state => state.latencyMs).filter(value => value !== null)).sort((a, b) => a - b);
  if (latency.length < seconds * 2) throw new Error(`too few decoded clock samples for ${name}`);
  const p95 = latency[Math.floor(latency.length * .95)];
  if (p95 > 10000) throw new Error(`balanced decoded live latency over budget in ${name}: ${p95} ms`);
  return { name, seconds, baseline, final, deltas, observations, decodedLatency: { samples: latency.length, p95Ms: p95, maxMs: latency.at(-1), quantizationMs: 100 } };
}
try {
  // First two seeds, then actual PeerTube recipients, then fresh native
  // receivers. All eight remain simultaneously active during measured phases.
  for (const [n, item] of manifest.entries()) await open('native', item.local, 0, n);
  for (const [n, item] of manifest.entries()) {
    await open('peertube', item.remote, 1500, n);
    await open('peertube', item.remote, 1500, n);
  }
  for (const [n, item] of manifest.entries()) {
    const receiver = await open('native', item.local, item.nativeWarmDelayMs ?? 4000, n);
    if(item.warmPeerDvr) await receiver.video.evaluate(video => {
      if(!video.seekable.length) throw new Error('source exposes no real rewind window');
      video.currentTime = Math.max(video.seekable.start(0), video.currentTime - 4);
    });
  }
  const began = Date.now();
  let native;
  while (Date.now() - began < 45000) {
    native = await Promise.all(clients.filter(client => client.kind === 'native').map(sample));
    if (manifest.every((_, broadcast) => native.filter((_, n) => clients.filter(client => client.kind === 'native')[n].broadcast === broadcast)
      .some(state => state.validated > 0)) && native.some(state => state.uploaded > 0)) break;
    await pause(500);
  }
  if (!manifest.every((_, broadcast) => native.filter((_, n) => clients.filter(client => client.kind === 'native')[n].broadcast === broadcast)
    .some(state => state.validated > 0)) || !native.some(state => state.uploaded > 0)) throw new Error(`concurrent peer bytes not established on both broadcasts: ${JSON.stringify(native)}`);
  checkpoint('both broadcasts exchanged peer bytes; remove warm-up routing');
  for (const client of clients) await client.page.unrouteAll({ behavior: 'ignoreErrors' });
  for (const client of clients) if(client.kind === 'native' && manifest[client.broadcast].warmPeerDvr) {
    const live = client.page.getByRole('button', { name: 'Go live', exact: true });
    if(await live.isVisible()) await live.click();
  }
  // Already-running warm-up delays are at most four seconds. Let those
  // requests finish outside the measured undelayed phase; don't wait forever
  // on Playwright's routing handler bookkeeping with continuously playing HLS.
  await pause(4500);
  observingSteady=true;
  const steady = await phase('undelayed steady', 120);
  progress.phases.push(steady);
  const before = await control('status');
  if (before.active_trackers < 8) throw new Error(`eight actual tracker connections not present: ${JSON.stringify(before)}`);
  const interrupted = await control('block');
  if (interrupted.tracker_interrupted < 8 || !interrupted.blocked) throw new Error('wire fault did not interrupt the real sockets');
  const outage = await phase('actual tracker TCP outage', 15);
  progress.phases.push(outage);
  const restored = await control('restore');
  const recovery = await phase('tracker restoration', 30);
  progress.phases.push(recovery);
  const reconnected = await control('status');
  if (reconnected.tracker_opened < before.tracker_opened + clients.length || reconnected.active_trackers < clients.length) {
    throw new Error(`not all viewers reconnected their tracker: ${JSON.stringify(reconnected)}`);
  }
  const result = { viewers: clients.length, broadcasts: manifest.length, warmingHttpDelayMs: [...new Set([1500, ...manifest.map(item => item.nativeWarmDelayMs ?? 4000)])],
    trackerEndpointMapping:manifest.map(item=>item.trackerVia?{original:item.trackerOrigin,via:item.trackerVia}:null),
    steady, before, interrupted, outage, restored, recovery, reconnected,
    sockets: clients.map(client => ({ kind: client.kind, broadcast: client.broadcast, sockets: client.sockets })) };
  writeFileSync(join(evidence, 'concurrent-live-browser.json'), JSON.stringify(result, null, 2), { mode: 0o600 });
  await clients[0].page.screenshot({ path: join(evidence, 'concurrent-live-native-320px.png'), fullPage: true });
  console.log(JSON.stringify({ viewers: result.viewers, broadcasts: result.broadcasts, before, interrupted, reconnected }));
  progress.status = 'passed';
} catch (error) {
  progress.status = 'failed';
  progress.error = String(error);
  checkpoint('failed; collecting diagnostic screenshots');
  progress.pages = await Promise.all(pages.map(async (item, n) => {
    await item.page.screenshot({ path: join(evidence, `failed-client-${n}.png`), fullPage: true }).catch(() => {});
    return { kind: item.kind, broadcast: item.broadcast, url: item.page.url(),
      title: await item.page.title().catch(() => null), errors: item.errors, failedRequests: item.failedRequests };
  }));
  progress.clients = await Promise.all(clients.map(async client => ({ kind: client.kind, broadcast: client.broadcast,
    sample: await sample(client).catch(() => null), sockets: client.sockets })));
  throw error;
} finally {
  if(wireObservers.length){
    progress.existingLiveResponses=(await Promise.all(wireObservers.map(finish=>finish()))).flat();
  }
  writeFileSync(join(evidence, 'concurrent-live-progress.json'), JSON.stringify(progress, null, 2), { mode: 0o600 });
  await control('restore').catch(() => {});
  for (const client of clients) await client.context.close().catch(() => {});
  await browser.close();
}
