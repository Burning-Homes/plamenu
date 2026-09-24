// Real-browser, cross-product proof for a Plamenu-origin native VOD swarm.
// PeerTube's own VideoJS player and Plamenu's own player exchange segment
// bytes over Plamenu's in-process tracker. The receiving Plamenu player must
// report checksum-validated playback, not just an attempted WebRTC transfer.
import { chromium } from 'playwright';

const [localUrl, peerTubeUrl] = process.argv.slice(2);
if (!localUrl || !peerTubeUrl) throw new Error('expected local watch URL and PeerTube watch URL');
const trackerUrl = `wss://${new URL(localUrl).host}/tracker/socket`;
const browser = await chromium.launch();
const errors = [];
const sockets = [];

function watch(page) {
  page.on('pageerror', error => errors.push(error.message));
  page.on('websocket', socket => {
    if (socket.url().includes('/tracker/socket')) sockets.push(socket.url());
  });
  page.on('response', response => {
    if (response.status() >= 400 && /\/media\/(hls|.*\.m4s)/.test(response.url())) {
      errors.push(`${response.status()} ${response.url()}`);
    }
  });
}

async function start(page, selector) {
  const video = page.locator(selector);
  await video.waitFor({ state: 'attached', timeout: 30000 });
  await video.evaluate(element => { element.muted = true; element.loop = true; element.play().catch(() => {}); });
  for (let i = 0; i < 120; i++) {
    if (await video.evaluate(element => element.currentTime > 0.25)) return video;
    await new Promise(resolve => setTimeout(resolve, 250));
  }
  const state = await video.evaluate(element => ({
    currentSrc: element.currentSrc, readyState: element.readyState,
    networkState: element.networkState, error: element.error?.message,
    paused: element.paused, currentTime: element.currentTime,
    sources: [...element.querySelectorAll('source')].map(source => source.src),
  }));
  throw new Error(`playback did not start for ${selector}: ${JSON.stringify(state)}; ${errors.join(' | ')}`);
}

try {
  const offContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const off = await offContext.newPage();
  watch(off);
  await off.goto(localUrl);
  const choice = off.locator('.media__p2p-toggle');
  await choice.waitFor({ state: 'visible' });
  if (!await choice.isChecked()) throw new Error('local-origin peer delivery did not default on');
  await choice.uncheck();
  await start(off, 'video[data-hls]');
  if (sockets.length || await off.locator('video[data-p2p-active]').count()) {
    throw new Error('local-origin pre-play opt-out still contacted the tracker');
  }
  await offContext.close();

  const localContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const local = await localContext.newPage();
  watch(local);
  await local.goto(localUrl);
  const localVideo = await start(local, 'video[data-hls]');
  await local.waitForFunction(() => !!document.querySelector('video[data-hls]')?.dataset.p2pInfohash,
    null, { timeout: 20000 });
  await local.waitForFunction(() => document.querySelector('video[data-hls]')?.currentTime > 8.5,
    null, { timeout: 30000 });
  const hash = await localVideo.getAttribute('data-p2p-infohash');
  const before = Number(await localVideo.getAttribute('data-p2p-uploaded') || 0);

  // Slowing only the origin HTTP fallback makes peer delivery observable,
  // while the normal fallback remains available if signaling fails.
  const peerContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const peer = await peerContext.newPage();
  watch(peer);
  if (process.env.PEERTUBE_E2E_USER && process.env.PEERTUBE_E2E_PASSWORD) {
    await peer.goto(new URL('/login', peerTubeUrl).href, { waitUntil: 'domcontentloaded' });
    await peer.locator('#username').fill(process.env.PEERTUBE_E2E_USER);
    await peer.locator('#password').fill(process.env.PEERTUBE_E2E_PASSWORD);
    await peer.locator('input[type="submit"][value="Login"]').click();
    await peer.waitForURL(url => url.pathname !== '/login', { timeout: 30000 });
  }
  await peer.route('**/media/*.m4s*', async route => {
    await new Promise(resolve => setTimeout(resolve, 4000));
    await route.continue();
  });
  await peer.goto(peerTubeUrl, { waitUntil: 'domcontentloaded' });
  const privacyNotice = peer.getByRole('button', { name: 'OK', exact: true });
  await privacyNotice.waitFor({ state: 'visible', timeout: 5000 }).catch(() => {});
  if (await privacyNotice.isVisible()) await privacyNotice.click();
  await peer.locator('video.vjs-tech').waitFor({ state: 'attached', timeout: 30000 });
  const bigPlay = peer.locator('.vjs-big-play-button');
  if (await bigPlay.isVisible()) await bigPlay.click();
  const peerVideo = await start(peer, 'video.vjs-tech');
  await local.waitForFunction(baseline =>
    Number(document.querySelector('video[data-hls]')?.dataset.p2pUploaded || 0) > baseline,
    before, { timeout: 30000 });
  await peer.waitForFunction(() => document.querySelector('video')?.currentTime > 8.5,
    null, { timeout: 30000 });
  const uploaded = Number(await localVideo.getAttribute('data-p2p-uploaded') || 0) - before;
  await localContext.close();

  // PeerTube is now the only seeded browser. A fresh Plamenu viewer must get
  // at least one full segment from it and pass the SHA-256 validator.
  const freshContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const fresh = await freshContext.newPage();
  watch(fresh);
  await fresh.route('**/media/*.m4s*', async route => {
    await new Promise(resolve => setTimeout(resolve, 4000));
    await route.continue();
  });
  await fresh.goto(localUrl);
  const freshVideo = await start(fresh, 'video[data-hls]');
  await fresh.waitForFunction(() => Number(document.querySelector('video[data-hls]')?.dataset.p2pValidated || 0) > 0,
    null, { timeout: 30000 });
  await fresh.waitForFunction(() => document.querySelector('video[data-hls]')?.currentTime > 8.5,
    null, { timeout: 30000 });
  const result = {
    infohash: hash,
    uploadedToPeerTube: uploaded,
    validatedFromPeerTube: Number(await freshVideo.getAttribute('data-p2p-validated') || 0),
    validationErrors: Number(await freshVideo.getAttribute('data-p2p-validation-errors') || 0),
    trackerSockets: sockets,
  };
  if (!sockets.some(url => url.startsWith(trackerUrl)) ||
      result.validationErrors || !result.uploadedToPeerTube || !result.validatedFromPeerTube) {
    throw new Error(`incomplete cross-product peer exchange: ${JSON.stringify(result)}; ${errors.join(' | ')}`);
  }
  console.log(JSON.stringify(result));
  await freshContext.close();
  await peerContext.close();
} finally {
  await browser.close();
}
