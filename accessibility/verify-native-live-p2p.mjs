// A real on-air Plamenu stream shared with PeerTube's unmodified player.
// Require checked segment bytes in both directions; HTTP remains a fallback.
import { chromium } from 'playwright';
import {openVideoSettings,closeVideoSettings} from './video-settings.mjs';

const [localUrl, peerTubeUrl] = process.argv.slice(2);
if (!localUrl || !peerTubeUrl) throw new Error('expected local and PeerTube watch URLs');
const browser = await chromium.launch();
const errors = [];
const sockets = [];
const segment = /\/media\/hls\/.*\/segment\d+\.ts(?:\?|$)/;
const observe = page => {
  page.on('pageerror', error => errors.push(error.message));
  page.on('response', response => {
    if (response.status() >= 400 && /\/media\/hls\//.test(response.url())) {
      errors.push(`${response.status()} ${response.url()}`);
    }
  });
  page.on('websocket', socket => {
    if (socket.url().includes('/tracker/socket')) sockets.push(socket.url());
  });
};
async function play(page, selector) {
  const video = page.locator(selector);
  await video.waitFor({ state: 'attached', timeout: 30000 });
  await video.evaluate(element => {
    element.muted = true;
    element.play().catch(() => {});
  });
  try {
    await page.waitForFunction(selector => document.querySelector(selector)?.currentTime > 0.25,
      selector, { timeout: 35000 });
  } catch (error) {
    const state = await video.evaluate(element => ({
      currentTime: element.currentTime, readyState: element.readyState,
      networkState: element.networkState, paused: element.paused,
      error: element.error?.message, currentSrc: element.currentSrc,
    }));
    throw new Error(`playback stalled: ${JSON.stringify(state)}; ${errors.join(' | ')}`, { cause: error });
  }
  return video;
}

try {
  const offContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const off = await offContext.newPage();
  observe(off);
  await off.goto(localUrl);
  const toggle = off.locator('.media__p2p-toggle');
  await openVideoSettings(off);
  await toggle.waitFor({ state: 'visible' });
  if (!await toggle.isChecked()) throw new Error('peer delivery did not default on');
  await toggle.uncheck();
  await closeVideoSettings(off);
  await play(off, 'video[data-hls]');
  if (sockets.length || await off.locator('video[data-p2p-active]').count()) {
    throw new Error('pre-play opt-out contacted the tracker');
  }
  await offContext.close();

  const noJsContext = await browser.newContext({ ignoreHTTPSErrors: true, javaScriptEnabled: false });
  const noJs = await noJsContext.newPage();
  await noJs.goto(localUrl);
  const mp4 = await noJs.locator('video[data-hls]').getAttribute('src');
  if (!mp4?.endsWith('/stream.mp4')) throw new Error('no-JS live MP4 source is missing');
  await noJsContext.close();

  const seedContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const seed = await seedContext.newPage();
  observe(seed);
  await seed.goto(localUrl);
  const seedVideo = await play(seed, 'video[data-hls]');
  await seed.waitForFunction(() => !!document.querySelector('video[data-hls]')?.dataset.p2pInfohash,
    null, { timeout: 25000 });
  await seed.waitForFunction(() => document.querySelector('video[data-hls]')?.currentTime > 4,
    null, { timeout: 35000 });
  const hash = await seedVideo.getAttribute('data-p2p-infohash');
  const baseline = Number(await seedVideo.getAttribute('data-p2p-uploaded') || 0);

  const peerContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const peer = await peerContext.newPage();
  observe(peer);
  if (process.env.PEERTUBE_E2E_USER && process.env.PEERTUBE_E2E_PASSWORD) {
    await peer.goto(new URL('/login', peerTubeUrl).href);
    await peer.locator('#username').fill(process.env.PEERTUBE_E2E_USER);
    await peer.locator('#password').fill(process.env.PEERTUBE_E2E_PASSWORD);
    await peer.locator('input[type="submit"][value="Login"]').click();
    await peer.waitForURL(url => url.pathname !== '/login', { timeout: 30000 });
  }
  await peer.goto(peerTubeUrl, { waitUntil: 'domcontentloaded' });
  const privacy = peer.getByRole('button', { name: 'OK', exact: true });
  await privacy.waitFor({ state: 'visible', timeout: 5000 }).catch(() => {});
  if (await privacy.isVisible()) await privacy.click();
  await peer.locator('video.vjs-tech').waitFor({ state: 'attached', timeout: 30000 });
  const bigPlay = peer.locator('.vjs-big-play-button');
  if (await bigPlay.isVisible()) await bigPlay.click();
  await play(peer, 'video.vjs-tech');
  // The live player first needs a short, undelayed buffer; adding delay at
  // startup can itself stall its low-latency target before P2P is negotiated.
  await peer.route(segment, async route => {
    await new Promise(resolve => setTimeout(resolve, 1500));
    await route.continue();
  });
  await seed.waitForFunction(before =>
    Number(document.querySelector('video[data-hls]')?.dataset.p2pUploaded || 0) > before,
    baseline, { timeout: 45000 });
  const uploaded = Number(await seedVideo.getAttribute('data-p2p-uploaded') || 0) - baseline;
  await seedContext.close();

  const freshContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const fresh = await freshContext.newPage();
  observe(fresh);
  await fresh.route(segment, async route => {
    await new Promise(resolve => setTimeout(resolve, 4000));
    await route.continue();
  });
  await fresh.goto(localUrl);
  const freshVideo = await play(fresh, 'video[data-hls]');
  await fresh.waitForFunction(() => Number(document.querySelector('video[data-hls]')?.dataset.p2pValidated || 0) > 0,
    null, { timeout: 45000 });
  const result = {
    infohash: hash,
    uploadedToPeerTube: uploaded,
    validatedFromPeerTube: Number(await freshVideo.getAttribute('data-p2p-validated') || 0),
    validationErrors: Number(await freshVideo.getAttribute('data-p2p-validation-errors') || 0),
    trackerSockets: sockets,
  };
  if (!sockets.some(url => url.startsWith(`wss://${new URL(localUrl).host}/tracker/socket`)) ||
      !result.uploadedToPeerTube || !result.validatedFromPeerTube || result.validationErrors) {
    throw new Error(`incomplete live peer exchange: ${JSON.stringify(result)}; ${errors.join(' | ')}`);
  }
  console.log(JSON.stringify(result));
  await freshContext.close();
  await peerContext.close();
} finally {
  await browser.close();
}
