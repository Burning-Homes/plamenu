// PeerTube-origin live P2P through the two unmodified first-party players.
// Plamenu checks every received peer segment against PeerTube's rolling SHA.
import { chromium } from 'playwright';
import { installPeerTubeWelcomeHandler } from './peertube-session.mjs';
import {openVideoSettings,closeVideoSettings} from './video-settings.mjs';

const [localUrl, peerTubeUrl, expectedHashesJson] = process.argv.slice(2);
if (!localUrl || !peerTubeUrl || !expectedHashesJson) {
  throw new Error('expected Plamenu and PeerTube watch URLs and AP infohashes');
}
const expectedHashes = JSON.parse(expectedHashesJson);
const browser = await chromium.launch();
const errors = [];
const sockets = [];
const segment = /\.ts(?:\?|$)/;
const transfers = [];
const playlists = [];

function filename(url) {
  const parsed = new URL(url);
  const encoded = parsed.pathname.match(/\/live\/([A-Za-z0-9_-]+)\.[A-Za-z0-9]+$/)?.[1];
  return new URL(encoded ? Buffer.from(encoded, 'base64url').toString('utf8') : url)
    .pathname.split('/').pop();
}

async function liveState(page, selector) {
  return page.locator(selector).evaluate(element => ({
    time: element.currentTime, ready: element.readyState, height: element.videoHeight,
    paused: element.paused, seeking: element.seeking,
    seekable: Array.from({length: Math.min(3, element.seekable.length)}, (_, i) =>
      [element.seekable.start(i), element.seekable.end(i)]),
    buffered: Array.from({length: Math.min(3, element.buffered.length)}, (_, i) =>
      [element.buffered.start(i), element.buffered.end(i)]),
    profile: window.__ownedVideoProfiles?.get(element)?.() || null,
  }));
}

function observe(page, player = 'other') {
  page.on('pageerror', error => errors.push(error.message));
  page.on('websocket', socket => {
    if (socket.url().includes('/tracker/socket')) sockets.push(socket.url());
  });
  page.on('response', response => {
    if (response.status() >= 400 && /\/media\/hls\//.test(response.url())) {
      errors.push(`${response.status()} ${response.url()}`);
    }
    if (segment.test(response.url())) {
      if (transfers.length >= 96) transfers.shift();
      transfers.push({player, name: filename(response.url()), status: response.status(),
        atMs: Date.now()});
    }
    if (/\.m3u8(?:\?|$)/.test(response.url())) {
      response.text().then(body => {
        const sequence = body.match(/^#EXT-X-MEDIA-SEQUENCE:(\d+)/m)?.[1];
        if (!sequence) return;
        if (playlists.length >= 16) playlists.shift();
        playlists.push({player, sequence: Number(sequence), atMs: Date.now(),
          segments: body.split('\n').filter(line => line && !line.startsWith('#'))
            .slice(-8).map(line => filename(new URL(line, response.url()).href))});
      }).catch(() => {});
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
  await installPeerTubeWelcomeHandler(page);
  observe(page, 'peertube');
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
  // Auto selects independently. Use the real quality controls to join the
  // same rendition, rather than expecting different swarms to exchange bytes.
  await page.locator('video.vjs-tech').hover({ force: true });
  await page.locator('.vjs-settings').click();
  await page.locator('.vjs-main-menu .vjs-menu-item').filter({ hasText: 'Quality' }).click();
  const quality = page.locator('.vjs-menu').filter({
    has: page.locator('.vjs-back-button').filter({ hasText: 'Quality' }),
  });
  await quality.locator('[role="menuitemradio"]', { hasText: '360p' }).click();
  await page.waitForFunction(() => document.querySelector('video.vjs-tech')?.videoHeight === 360,
    null, { timeout: 15000 });
  return page;
}

try {
  const offContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const off = await offContext.newPage();
  observe(off);
  await off.goto(localUrl);
  const toggle = off.locator('.media__p2p-toggle');
  await openVideoSettings(off);
  await toggle.waitFor({ state: 'visible' });
  if (!await toggle.isChecked()) throw new Error('remote-live peer delivery did not default on');
  await toggle.uncheck();
  await closeVideoSettings(off);
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
  await localContext.addInitScript(() => { window.__ownedVideoProfile = true; });
  const local = await localContext.newPage();
  observe(local, 'plamenu');
  await local.goto(localUrl);
  const localVideo = await play(local, 'video[data-hls]');
  await local.waitForFunction(() => !!document.querySelector('video[data-hls]')?.dataset.p2pInfohash,
    null, { timeout: 25000 });
  const localQuality = local.getByRole('combobox', { name: 'Video quality' });
  if (await localQuality.count()) await localQuality.selectOption({ label: '360p' });
  await local.waitForFunction(() => document.querySelector('video[data-hls]')?.videoHeight === 360,
    null, { timeout: 15000 });
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
  // Plamenu's live edge can lead PeerTube's playback buffer. A seeder cannot
  // supply future bytes. Rewind through the native player's seek surface into
  // its already-decoded window before testing inbound delivery. Production
  // latency and peer/HTTP deadlines stay unchanged.
  const seedTime = await seed.locator('video.vjs-tech').evaluate(element => element.currentTime);
  const beforeSeek = {
    local: await liveState(local, 'video[data-hls]'),
    peer: await liveState(seed, 'video.vjs-tech'),
  };
  const rewind = await localVideo.evaluate((element, peerTime) => {
    if (!element.seekable.length) throw new Error('live player has no seekable window');
    const position = Math.max(element.seekable.start(0) + 0.5, peerTime - 6);
    if (position >= peerTime) throw new Error('the peer has no overlapping seeded live window');
    element.currentTime = position;
    element.play().catch(() => {});
    return position;
  }, seedTime);
  // A cold HLS player can expose its pending live-edge seek before the
  // first decoded frame. A deliberate backward seek must still survive
  // live-edge recovery rather than silently jumping back to future bytes.
  await local.waitForFunction(position => {
    const video = document.querySelector('video[data-hls]');
    return video?.readyState >= 3 && !video.seeking &&
      video.getVideoPlaybackQuality().totalVideoFrames >= 8 &&
      video.currentTime >= position && video.currentTime < position + 4;
  }, rewind, { timeout: 15000 });
  const rewindDecoded = await liveState(local, 'video[data-hls]');
  if (rewindDecoded.seekable.at(-1)[1] - rewindDecoded.time < 8) {
    throw new Error(`live-edge recovery undid the deliberate rewind: ${JSON.stringify({
      beforeSeek, rewind, rewindDecoded,
    })}`);
  }
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
    throw new Error(`no validated PeerTube live segment: ${JSON.stringify({ state, peerState,
      beforeSeek, rewind, local: await liveState(local, 'video[data-hls]'),
      peer: await liveState(seed, 'video.vjs-tech'), transfers, playlists,
      delayedLocal, sockets, errors })}`, { cause: error });
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
    overlappingSeedTime: seedTime,
    rewindPosition: rewind,
    rewindDecoded,
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
