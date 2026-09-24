// PeerTube-origin live P2P through the two unmodified first-party players.
// Plamenu checks every received peer segment against PeerTube's rolling SHA.
import { chromium } from 'playwright';

const [localUrl, peerTubeUrl, expectedHashesJson] = process.argv.slice(2);
if (!localUrl || !peerTubeUrl || !expectedHashesJson) {
  throw new Error('expected Plamenu and PeerTube watch URLs and AP infohashes');
}
const expectedHashes = JSON.parse(expectedHashesJson);
const browser = await chromium.launch();
const errors = [];
const sockets = [];
const segment = /\.ts(?:\?|$)/;

function observe(page) {
  page.on('pageerror', error => errors.push(error.message));
  page.on('websocket', socket => {
    if (socket.url().includes('/tracker/socket')) sockets.push(socket.url());
  });
  page.on('response', response => {
    if (response.status() >= 400 && /\/media\/hls\//.test(response.url())) {
      errors.push(`${response.status()} ${response.url()}`);
    }
  });
}

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
    throw new Error(`live playback stalled: ${JSON.stringify(state)}; ${errors.join(' | ')}`, { cause: error });
  }
  return video;
}

async function peerTubePage(context) {
  const page = await context.newPage();
  observe(page);
  if (process.env.PEERTUBE_E2E_USER && process.env.PEERTUBE_E2E_PASSWORD) {
    await page.goto(new URL('/login', peerTubeUrl).href);
    await page.locator('#username').fill(process.env.PEERTUBE_E2E_USER);
    await page.locator('#password').fill(process.env.PEERTUBE_E2E_PASSWORD);
    await page.locator('input[type="submit"][value="Login"]').click();
    await page.waitForURL(url => url.pathname !== '/login', { timeout: 30000 });
  }
  await page.goto(peerTubeUrl, { waitUntil: 'domcontentloaded' });
  const privacy = page.getByRole('button', { name: 'OK', exact: true });
  await privacy.waitFor({ state: 'visible', timeout: 5000 }).catch(() => {});
  if (await privacy.isVisible()) await privacy.click();
  await page.locator('video.vjs-tech').waitFor({ state: 'attached', timeout: 30000 });
  const bigPlay = page.locator('.vjs-big-play-button');
  if (await bigPlay.isVisible()) await bigPlay.click();
  await play(page, 'video.vjs-tech');
  return page;
}

try {
  const offContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const off = await offContext.newPage();
  observe(off);
  await off.goto(localUrl);
  const toggle = off.locator('.media__p2p-toggle');
  await toggle.waitFor({ state: 'visible' });
  if (!await toggle.isChecked()) throw new Error('remote-live peer delivery did not default on');
  await toggle.uncheck();
  await play(off, 'video[data-hls]');
  if (sockets.length || await off.locator('video[data-p2p-active]').count()) {
    throw new Error('remote-live pre-play opt-out contacted a tracker');
  }
  await offContext.close();

  const noJsContext = await browser.newContext({ ignoreHTTPSErrors: true, javaScriptEnabled: false });
  const noJs = await noJsContext.newPage();
  await noJs.goto(localUrl);
  const mp4 = await noJs.locator('video[data-hls]').getAttribute('src');
  if (!mp4?.endsWith('/stream.mp4')) throw new Error('no-JS MP4 fallback is missing');
  await noJsContext.close();

  const seedContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const seed = await peerTubePage(seedContext);
  await seed.waitForFunction(() => document.querySelector('video.vjs-tech')?.currentTime > 4,
    null, { timeout: 35000 });

  const localContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const local = await localContext.newPage();
  observe(local);
  await local.goto(localUrl);
  const localVideo = await play(local, 'video[data-hls]');
  await local.waitForFunction(() => !!document.querySelector('video[data-hls]')?.dataset.p2pInfohash,
    null, { timeout: 25000 });
  const hash = await localVideo.getAttribute('data-p2p-infohash');
  if (!expectedHashes.includes(hash)) {
    throw new Error(`Plamenu joined ${hash}, not PeerTube's AP swarms ${expectedHashes.join(', ')}`);
  }
  let delayedLocal = 0;
  await local.route(segment, async route => {
    delayedLocal++;
    await new Promise(resolve => setTimeout(resolve, 1500));
    await route.continue();
  });
  try {
    await local.waitForFunction(() => Number(document.querySelector('video[data-hls]')?.dataset.p2pValidated || 0) > 0,
      null, { timeout: 45000 });
  } catch (error) {
    const state = await localVideo.evaluate(element => ({
      time: element.currentTime, ready: element.readyState, data: { ...element.dataset },
    }));
    const peerState = await seed.locator('video.vjs-tech').evaluate(element => ({
      time: element.currentTime, ready: element.readyState,
    }));
    throw new Error(`no validated PeerTube live segment: ${JSON.stringify({ state, peerState, delayedLocal, sockets, errors })}`, { cause: error });
  }
  const validated = Number(await localVideo.getAttribute('data-p2p-validated') || 0);
  const validationErrors = Number(await localVideo.getAttribute('data-p2p-validation-errors') || 0);
  const baselineUpload = Number(await localVideo.getAttribute('data-p2p-uploaded') || 0);
  await seedContext.close();

  const freshContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const fresh = await peerTubePage(freshContext);
  await fresh.route(segment, async route => {
    await new Promise(resolve => setTimeout(resolve, 1500));
    await route.continue();
  });
  await local.waitForFunction(before =>
    Number(document.querySelector('video[data-hls]')?.dataset.p2pUploaded || 0) > before,
    baselineUpload, { timeout: 45000 });
  const uploaded = Number(await localVideo.getAttribute('data-p2p-uploaded') || 0) - baselineUpload;
  const result = {
    infohash: hash,
    validatedFromPeerTube: validated,
    uploadedToPeerTube: uploaded,
    validationErrors,
    trackerSockets: sockets,
  };
  if (!sockets.some(url => {
        const tracker = new URL(url);
        return tracker.protocol === 'wss:' &&
          tracker.hostname === new URL(peerTubeUrl).hostname &&
          tracker.pathname === '/tracker/socket';
      }) ||
      !validated || !uploaded || validationErrors) {
    throw new Error(`incomplete PeerTube-origin live exchange: ${JSON.stringify(result)}; ${errors.join(' | ')}`);
  }
  console.log(JSON.stringify(result));
  await freshContext.close();
  await localContext.close();
} finally {
  await browser.close();
}
