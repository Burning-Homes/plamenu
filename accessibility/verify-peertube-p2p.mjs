// Real-browser PeerTube HLS peer interoperability and the pre-contact opt-out.
// Called with a public Plamenu status URL, Infohash names from its signed
// PeerTube Video object, an output screenshot and the PeerTube watch URL.
// The second Plamenu browser and then PeerTube's own player join the swarm.
import { chromium } from 'playwright';
import {openVideoSettings,closeVideoSettings} from './video-settings.mjs';
import AxeBuilder from '@axe-core/playwright';

const [url, infohashesJson, screenshot, peerTubeUrl] = process.argv.slice(2);
if (!url || !infohashesJson || !screenshot || !peerTubeUrl) {
  throw new Error('expected Plamenu URL, infohash JSON, screenshot path and PeerTube URL');
}
const expected = JSON.parse(infohashesJson);
const browser = await chromium.launch();
const trackerSockets = [];
const browserErrors = [];

function watchRequests(page) {
  page.on('pageerror', error => browserErrors.push(`page: ${error.message}`));
  page.on('console', message => {
    if (message.type() === 'error') browserErrors.push(`console: ${message.text()}`);
  });
  page.on('requestfailed', request => browserErrors.push(`request: ${request.url()} ${request.failure()?.errorText}`));
  page.on('response', response => {
    if (response.status() >= 400 && (response.url().includes('/media/hls/') || response.url().includes('peertube.local'))) {
      browserErrors.push(`HTTP ${response.status()}: ${response.url()}`);
    }
  });
}

async function startVideo(page) {
  await page.locator('video[data-hls]').evaluate((video) => {
    video.muted = true;
    video.loop = true;
    // app.js intentionally pauses the first play to exchange the progressive
    // fallback for HLS, then resumes. The first play promise is aborted.
    video.play().catch(() => {});
  });
  try {
    // Host-side polling also works in a context with page JavaScript disabled;
    // page.waitForFunction itself does not run there.
    let advanced = false;
    for (let attempt = 0; attempt < 120; attempt++) {
      if (await page.locator('video[data-hls]').evaluate(video => video.currentTime > 0.25)) {
        advanced = true;
        break;
      }
      await new Promise(resolve => setTimeout(resolve, 250));
    }
    if (!advanced) throw new Error('30-second playback timeout');
  } catch (error) {
    const state = await page.locator('video[data-hls]').evaluate(video => ({
      paused: video.paused, currentSrc: video.currentSrc, currentTime: video.currentTime,
      networkState: video.networkState, readyState: video.readyState,
      error: video.error?.message, dataset: { ...video.dataset },
      hlsLoaded: !!window.Hls,
    }));
    throw new Error(`playback did not advance: ${JSON.stringify(state)}; ${browserErrors.join(' | ')}; ${error.message}`);
  }
}

try {
  const noJSContext = await browser.newContext({ ignoreHTTPSErrors: true, javaScriptEnabled: false });
  const noJS = await noJSContext.newPage();
  noJS.on('websocket', socket => trackerSockets.push(socket.url()));
  await noJS.goto(url);
  if (await noJS.locator('.media__p2p-toggle').isVisible()) {
    throw new Error('scripts-disabled viewer was offered an inoperable peer control');
  }
  await startVideo(noJS);
  const noJSSource = await noJS.locator('video[data-hls]').evaluate(video => video.currentSrc);
  if (!noJSSource.includes('/media/play/')) throw new Error(`scripts-disabled playback lost MP4 fallback: ${noJSSource}`);
  await noJSContext.close();

  const offContext = await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 320, height: 800 } });
  const off = await offContext.newPage();
  watchRequests(off);
  off.on('websocket', socket => trackerSockets.push(socket.url()));
  await off.goto(url);
  const control = off.locator('.media__p2p-toggle');
  await openVideoSettings(off);
  await control.waitFor({ state: 'visible' });
  await control.uncheck();
  await closeVideoSettings(off);
  await startVideo(off);
  if (trackerSockets.some(socket => socket.includes('/tracker/'))) {
    throw new Error(`opted-out viewer contacted a tracker: ${trackerSockets}`);
  }
  if (await off.locator('video[data-p2p-active]').count()) {
    throw new Error('opted-out viewer constructed a peer engine');
  }
  if (!await off.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)) {
    throw new Error('peer control overflows a 320px viewport');
  }
  await off.screenshot({ path: screenshot, fullPage: true });
  const violations = (await new AxeBuilder({ page: off }).include('figure.media--video')
    .withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze()).violations;
  if (violations.length) throw new Error(`peer choice accessibility violations: ${violations.map(v => v.id).join(', ')}`);
  await off.reload();
  await control.waitFor({ state: 'visible' });
  if (await control.isChecked()) throw new Error('pre-play opt-out did not persist in this browser');
  await offContext.close();

  const firstContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const secondContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const first = await firstContext.newPage();
  const second = await secondContext.newPage();
  let secondTrackerClosed = false;
  // Give the already-buffered peer a chance to answer before this viewer's
  // ordinary HTTP fallback. The fallback still runs if P2P cannot deliver.
  await second.route('**/media/hls/**/seg?*', async route => {
    await new Promise(resolve => setTimeout(resolve, 4000));
    await route.continue();
  });
  for (const page of [first, second]) {
    watchRequests(page);
    page.on('websocket', socket => {
      trackerSockets.push(socket.url());
      if (page === second && socket.url().includes('/tracker/socket')) {
        socket.on('close', () => { secondTrackerClosed = true; });
      }
    });
    await page.goto(url);
    if (!await page.locator('.media__p2p-toggle').isChecked()) {
      throw new Error('peer delivery did not default on in a new browser context');
    }
  }
  await startVideo(first);
  try {
    await first.waitForFunction(() => !!document.querySelector('video[data-hls]')?.dataset.p2pInfohash,
      null, { timeout: 20000 });
  } catch (error) {
    const state = await first.locator('video[data-hls]').evaluate(video => ({
      dataset: { ...video.dataset }, p2pLoaded: !!window.p2pml,
      p2pHls: !!window.p2pml?.hlsjs, hlsLoaded: !!window.Hls,
    }));
    throw new Error(`peer engine did not register a stream: ${JSON.stringify(state)}; ${browserErrors.join(' | ')}; ${error.message}`);
  }
  const hash = await first.locator('video[data-hls]').getAttribute('data-p2p-infohash');
  if (!expected.includes(hash)) throw new Error(`swarm infohash ${hash} differs from PeerTube announcement ${expected}`);
  await startVideo(second);
  await first.waitForFunction(() => Number(document.querySelector('video[data-hls]')?.dataset.p2pPeers || 0) > 0,
    null, { timeout: 30000 });
  await second.waitForFunction(() => Number(document.querySelector('video[data-hls]')?.dataset.p2pPeers || 0) > 0,
    null, { timeout: 30000 });
  await second.waitForFunction(() => Number(document.querySelector('video[data-hls]')?.dataset.p2pValidated || 0) > 0,
    null, { timeout: 30000 });
  if (Number(await second.locator('video[data-hls]').getAttribute('data-p2p-validation-errors') || 0) !== 0) {
    throw new Error('peer segment failed checksum validation');
  }
  if (!trackerSockets.some(socket => socket.includes('/tracker/socket'))) {
    throw new Error(`no PeerTube tracker connection: ${trackerSockets}`);
  }
  console.log(JSON.stringify({ infohash: hash, trackerSockets,
    first: await first.locator('video[data-hls]').evaluate(v => ({ peers: v.dataset.p2pPeers, downloaded: v.dataset.p2pDownloaded, uploaded: v.dataset.p2pUploaded })),
    second: await second.locator('video[data-hls]').evaluate(v => ({ peers: v.dataset.p2pPeers, downloaded: v.dataset.p2pDownloaded, validated: v.dataset.p2pValidated, uploaded: v.dataset.p2pUploaded })) }));
  await openVideoSettings(second);
  await second.locator('.media__p2p-toggle').uncheck();
  await closeVideoSettings(second);
  for (let attempt = 0; attempt < 40 && !secondTrackerClosed; attempt++) {
    await new Promise(resolve => setTimeout(resolve, 250));
  }
  if (!secondTrackerClosed) throw new Error('turning peer sharing off did not close the active tracker socket');
  await secondContext.close();
  const uploadedBeforePeerTube = Number(await first.locator('video[data-hls]').getAttribute('data-p2p-uploaded') || 0);

  // PeerTube's own first-party player now joins the Plamenu seed. Slow only
  // its segment HTTP fallback so a completed upload from Plamenu proves the
  // reverse client direction rather than a same-origin cache hit.
  const peerTubeContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const peerTube = await peerTubeContext.newPage();
  const peerTubeTrackerSockets = [];
  peerTube.on('websocket', socket => {
    if (socket.url().includes('/tracker/socket')) peerTubeTrackerSockets.push(socket.url());
  });
  watchRequests(peerTube);
  await peerTube.route('**/*-fragmented.mp4*', async route => {
    await new Promise(resolve => setTimeout(resolve, 4000));
    await route.continue();
  });
  await peerTube.goto(peerTubeUrl, { waitUntil: 'domcontentloaded' });
  await peerTube.locator('video').waitFor({ state: 'attached', timeout: 30000 });
  await peerTube.waitForFunction(() => !!document.querySelector('video')?.currentSrc,
    null, { timeout: 30000 });
  // VideoJS may auto-start and hide its large Play button in an isolated
  // context. Drive its native media element once its HLS source is bound.
  await peerTube.locator('video').evaluate(video => {
    video.muted = true;
    video.loop = true;
    video.play().catch(() => {});
  });
  for (let attempt = 0; attempt < 80; attempt++) {
    if (await peerTube.locator('video').evaluate(video => video.currentTime > 0.25)) break;
    await new Promise(resolve => setTimeout(resolve, 250));
  }
  if (!await peerTube.locator('video').evaluate(video => video.currentTime > 0.25)) {
    throw new Error(`PeerTube first-party player did not start: ${browserErrors.join(' | ')}`);
  }
  for (let attempt = 0; attempt < 40 && !peerTubeTrackerSockets.length; attempt++) {
    await new Promise(resolve => setTimeout(resolve, 250));
  }
  if (!peerTubeTrackerSockets.length) throw new Error('PeerTube player did not join its own tracker');
  try {
    await first.waitForFunction(baseline =>
      Number(document.querySelector('video[data-hls]')?.dataset.p2pUploaded || 0) > baseline,
      uploadedBeforePeerTube, { timeout: 30000 });
  } catch (error) {
    const state = await first.locator('video[data-hls]').evaluate(video => ({ ...video.dataset }));
    throw new Error(`PeerTube player did not fetch from Plamenu: ${JSON.stringify(state)}; ${peerTubeTrackerSockets}; ${browserErrors.join(' | ')}; ${error.message}`);
  }
  const peerTubeReceived = Number(await first.locator('video[data-hls]').getAttribute('data-p2p-uploaded')) - uploadedBeforePeerTube;
  await peerTube.waitForFunction(() => document.querySelector('video')?.currentTime > 8.5,
    null, { timeout: 30000 });
  await firstContext.close();

  // With Plamenu's seed gone, PeerTube is the only other peer available to a
  // fresh Plamenu viewer. Require a fully checksum-validated segment from it.
  const crossContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const cross = await crossContext.newPage();
  const crossSockets = [];
  cross.on('websocket', socket => crossSockets.push(socket.url()));
  watchRequests(cross);
  await cross.route('**/media/hls/**/seg?*', async route => {
    await new Promise(resolve => setTimeout(resolve, 4000));
    await route.continue();
  });
  await cross.goto(url);
  await startVideo(cross);
  try {
    await cross.waitForFunction(() => Number(document.querySelector('video[data-hls]')?.dataset.p2pValidated || 0) > 0,
      null, { timeout: 30000 });
  } catch (error) {
    const state = await cross.locator('video[data-hls]').evaluate(video => ({ ...video.dataset }));
    throw new Error(`no validated PeerTube-player → Plamenu segment: ${JSON.stringify(state)}; ${crossSockets}; ${browserErrors.join(' | ')}; ${error.message}`);
  }
  await cross.waitForFunction(() => document.querySelector('video[data-hls]')?.currentTime > 8.5,
    null, { timeout: 30000 });
  console.log(JSON.stringify({ peerTubePlayer: true, peerTubeTrackerSockets,
    plamenuToPeerTube: peerTubeReceived,
    peerTubeToPlamenu: await cross.locator('video[data-hls]').getAttribute('data-p2p-validated') }));
  await crossContext.close();
  await peerTubeContext.close();
} finally {
  await browser.close();
}
