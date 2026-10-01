// Actual decoded offloaded output in the shipped native (enhanced and genuine
// app-JS-disabled) and unmodified PeerTube players. No media interception.
import { chromium } from 'playwright';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
const [manifestPath, evidence] = process.argv.slice(2);
const publications = JSON.parse(readFileSync(manifestPath, 'utf8'));
const browser = await chromium.launch();
const results = [];
let auth;
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
async function sample(video) {
  return video.evaluate(v => ({ time: v.currentTime, paused: v.paused,
    duration: v.duration, ready: v.readyState, seeking: v.seeking, error: v.error?.code || null,
    frames: v.getVideoPlaybackQuality().totalVideoFrames,
    dropped: v.getVideoPlaybackQuality().droppedVideoFrames }));
}
async function decoded(video, previous, timeout) {
  const begin = Date.now();
  while (Date.now() - begin < timeout) {
    const result = await sample(video);
    if (result.error) throw new Error(`decoder error ${result.error}`);
    if (!result.paused && result.ready >= 2 && result.frames >= previous + 3) return result;
    await pause(50);
  }
  throw new Error(`decode timeout: ${JSON.stringify(await sample(video))}`);
}
try {
  for (const [index, publication] of publications.entries()) {
    for (const kind of ['native-js', 'native-no-js', 'peertube']) {
      const peer = kind === 'peertube';
      const context = await browser.newContext({ ignoreHTTPSErrors: true,
        javaScriptEnabled: kind !== 'native-no-js', viewport: { width: peer ? 800 : 320, height: 800 },
        ...(peer && auth ? { storageState: auth } : {}) });
      try {
        const page = await context.newPage();
        const errors = [];
        page.on('pageerror', error => errors.push(String(error)));
        if (peer && !auth) {
          await page.goto(new URL('/login', publication.peertube_url).href);
          await page.locator('#username').fill(process.env.PEERTUBE_E2E_USER);
          await page.locator('#password').fill(process.env.PEERTUBE_E2E_PASSWORD);
          await page.locator('input[type="submit"][value="Login"]').click();
          await page.waitForURL(url => url.pathname !== '/login');
          auth = await context.storageState(); // Only process memory, never evidence.
        }
        await page.goto(peer ? publication.peertube_url : publication.native_url, { waitUntil: 'domcontentloaded' });
        if (peer) {
          const consent = page.getByRole('button', { name: 'OK', exact: true });
          await consent.waitFor({ state: 'visible', timeout: 3000 }).catch(() => {});
          if (await consent.isVisible()) await consent.click();
        }
        const video = page.locator(peer ? 'video.vjs-tech' : 'video[data-hls]').first();
        await video.waitFor({ state: 'attached' });
        if (peer && await page.locator('.vjs-big-play-button').isVisible()) await page.locator('.vjs-big-play-button').click();
        const started = Date.now();
        // External instrumentation of the native media API is not application
        // script support: the no-JS context stays disabled throughout.
        await video.evaluate(v => { v.muted = true; v.loop = true; v.play().catch(() => {}); });
        const initial = await decoded(video, 0, 10000);
        const startupMs = Date.now() - started;
        const seekStarted = Date.now();
        await video.evaluate(v => { v.currentTime = 3; });
        let seek;
        while (Date.now() - seekStarted < 3000) {
          seek = await sample(video);
          if (seek.time >= 3 && seek.time < 5 && !seek.seeking && seek.ready >= 2 && seek.frames >= initial.frames + 3 && !seek.error) break;
          await pause(50);
        }
        if (!(seek.time >= 3 && seek.time < 5 && !seek.seeking && seek.ready >= 2 && seek.frames >= initial.frames + 3 && !seek.error)) throw new Error(`seek stalled ${kind}`);
        const seekMs = Date.now() - seekStarted;
        await pause(1500);
        const steady = await decoded(video, seek.frames, 3000);
        const reflow = await page.evaluate(() => ({ width: innerWidth, scroll: document.documentElement.scrollWidth }));
        if (reflow.scroll > reflow.width || errors.length || steady.dropped || steady.error) throw new Error(`playback/reflow failure ${kind}: ${JSON.stringify({ errors, steady, reflow })}`);
        await page.screenshot({ path: join(evidence, `offload-${index}-${kind}.png`), fullPage: false });
        results.push({ publication: index, kind, initial, seek, steady, startupMs, seekMs, reflow, errors });
      } finally { await context.close(); }
    }
  }
  writeFileSync(join(evidence, 'offload-playback.json'), JSON.stringify(results, null, 2), { mode: 0o600 });
} finally { await browser.close(); }
