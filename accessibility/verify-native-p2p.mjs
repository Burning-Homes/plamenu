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
const responses = [];

function watch(page) {
  page.on('pageerror', error => errors.push(error.message));
  page.on('websocket', socket => {
    if (socket.url().includes('/tracker/socket')) sockets.push(socket.url());
  });
  page.on('response', response => {
    if (/\/media\//.test(response.url())) responses.push({url:response.url(),status:response.status()});
    if (response.status() >= 400 && /\/media\/(hls|.*\.m4s)/.test(response.url())) {
      errors.push(`${response.status()} ${response.url()}`);
    }
  });
  page.on('requestfailed', request => errors.push(`${request.failure()?.errorText} ${request.url()}`));
}

async function instrument(page) {
  await page.addInitScript(() => {
    const observations = window.__nativeHlsObservations = [];
    let player;
    Object.defineProperty(window, 'Hls', {
      configurable: true,
      get: () => player,
      set: Hls => {
        player = class extends Hls {
          constructor(...args) {
            super(...args);
            for (const event of [Hls.Events.ERROR, Hls.Events.LEVEL_SWITCHING, Hls.Events.FRAG_BUFFERED]) {
              this.on(event, (_event, data) => {
                observations.push({event, type:data.type,details:data.details,fatal:data.fatal,
                  error:data.error?.message,level:data.level,fragLevel:data.frag?.level,sn:data.frag?.sn});
                if (observations.length > 100) observations.shift();
              });
            }
          }
        };
      },
    });
  });
}

async function start(page, selector, qualityHeight = null) {
  const video = page.locator(selector);
  await video.waitFor({ state: 'attached', timeout: 30000 });
  await video.evaluate(element => { element.muted = true; element.loop = true; element.play().catch(() => {}); });
  if (qualityHeight) {
    const quality = page.getByRole('combobox', {name:'Video quality',exact:true});
    await quality.waitFor({state:'visible',timeout:20000});
    await quality.selectOption({label:`${qualityHeight}p`});
  }
  for (let i = 0; i < 120; i++) {
    if (await video.evaluate(element => element.currentTime > 0.25)) return video;
    await new Promise(resolve => setTimeout(resolve, 250));
  }
  const state = await video.evaluate(element => ({
    currentSrc: element.currentSrc, readyState: element.readyState,
    networkState: element.networkState, error: element.error?.message,
    paused: element.paused, currentTime: element.currentTime,
    sources: [...element.querySelectorAll('source')].map(source => source.src),
    buffered: [...Array(element.buffered.length)].map((_,i)=>[element.buffered.start(i),element.buffered.end(i)]),
    peer: {...element.dataset},
    hls: window.__nativeHlsObservations,
  }));
  throw new Error(`playback did not start for ${selector}: ${JSON.stringify(state)}; ${errors.join(' | ')}; responses=${JSON.stringify(responses.slice(-30))}`);
}

try {
  const offContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const off = await offContext.newPage();
  await instrument(off);
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

  // Auto must still decode with no seeder when origin time-to-first-byte
  // exceeds the peer library's stock 3s limit. This is separate from proving
  // byte exchange, which requires participants in the same rendition swarm.
  const slowContext = await browser.newContext({ ignoreHTTPSErrors:true });
  const slow = await slowContext.newPage();
  watch(slow);
  await slow.route('**/media/*.m4s*',async route=>{
    await new Promise(resolve=>setTimeout(resolve,4000));
    await route.continue();
  });
  await slow.goto(localUrl);
  const slowStart = Date.now();
  await start(slow,'video[data-hls]');
  const slowAutoStartupMs = Date.now()-slowStart;
  await slowContext.close();

  const localContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const local = await localContext.newPage();
  await instrument(local);
  watch(local);
  await local.goto(localUrl);
  const localVideo = await start(local, 'video[data-hls]', 480);
  await local.waitForFunction(()=>document.querySelector('video[data-hls]')?.videoHeight===480,
    null,{timeout:15000});
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
  // Auto chooses independently in each product. Deliberately align the
  // quality through the real controls, rather than assuming all new rungs
  // are seeded by a browser playing one representation.
  await peerVideo.hover({force:true});
  await peer.locator('.vjs-settings').click();
  await peer.locator('.vjs-main-menu .vjs-menu-item').filter({hasText:'Quality'}).click();
  const qualityMenu = peer.locator('.vjs-menu').filter({
    has:peer.locator('.vjs-back-button').filter({hasText:'Quality'}),
  });
  await qualityMenu.locator('[role="menuitemradio"]',{hasText:'480p'}).click();
  await peerVideo.evaluate(video=>{video.currentTime=0;video.play().catch(()=>{});});
  await peer.waitForFunction(()=>document.querySelector('video.vjs-tech')?.videoHeight===480,
    null,{timeout:15000});
  await local.waitForFunction(baseline =>
    Number(document.querySelector('video[data-hls]')?.dataset.p2pUploaded || 0) > baseline,
    before, { timeout: 30000 });
  await peer.waitForFunction(() => document.querySelector('video')?.currentTime > 18.5,
    null, { timeout: 30000 });
  const uploaded = Number(await localVideo.getAttribute('data-p2p-uploaded') || 0) - before;
  await localContext.close();

  // PeerTube is now the only seeded browser. A fresh Plamenu viewer must get
  // at least one full segment from it and pass the SHA-256 validator.
  const freshContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const fresh = await freshContext.newPage();
  await instrument(fresh);
  watch(fresh);
  await fresh.route('**/media/*.m4s*', async route => {
    // Initialization is not exchanged through the swarm. Keep it immediate,
    // and give ICE discovery (up to 5s) time to find the seeded representation
    // before the still-available, bounded HTTP media fallback wins the race.
    if (!route.request().headers().range?.startsWith('bytes=0-')) {
      await new Promise(resolve => setTimeout(resolve, 8000));
    }
    await route.continue();
  });
  await fresh.goto(localUrl);
  const freshVideo = await start(fresh, 'video[data-hls]', 480);
  try {
    await fresh.waitForFunction(() => Number(document.querySelector('video[data-hls]')?.dataset.p2pValidated || 0) > 0,
      null, { timeout: 30000 });
  } catch (error) {
    const receiver = await freshVideo.evaluate(video=>({dataset:{...video.dataset},time:video.currentTime,
      width:video.videoWidth,readyState:video.readyState,hls:window.__nativeHlsObservations}));
    const sender = await peerVideo.evaluate(video=>({src:video.currentSrc,time:video.currentTime,
      width:video.videoWidth,readyState:video.readyState}));
    throw new Error(`no validated peer segment: ${JSON.stringify({receiver,sender})}; ${error.message}`);
  }
  await fresh.waitForFunction(() => document.querySelector('video[data-hls]')?.currentTime > 8.5,
    null, { timeout: 30000 });
  const result = {
    qualityHeight:480,
    slowAutoStartupMs,
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
