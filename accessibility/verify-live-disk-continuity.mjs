// Three actual decoders span an owned real filesystem fault. Application
// scripts remain disabled in the no-JS context. No media/tracker interception.
import { chromium } from 'playwright';
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
const [manifestPath, evidence] = process.argv.slice(2);
const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
const browser = await chromium.launch();
const clients = [];
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
const report = { phases: [], status: 'running' };
const reportPath = join(evidence, 'disk-browser.json');
function save(stage) { report.stage = stage; writeFileSync(reportPath, JSON.stringify(report, null, 2), { mode: 0o600 }); }
async function sample(client) {
  return client.video.evaluate(v => ({ time: v.currentTime, ready: v.readyState,
    frames: v.getVideoPlaybackQuality().totalVideoFrames, dropped: v.getVideoPlaybackQuality().droppedVideoFrames,
    error: v.error?.code || null, paused: v.paused, validated: Number(v.dataset.p2pValidated || 0),
    integrityErrors: Number(v.dataset.p2pValidationErrors || 0) }));
}
async function phase(name, seconds) {
  const initial = await Promise.all(clients.map(sample));
  await pause(seconds * 1000);
  const final = await Promise.all(clients.map(sample));
  for (let index = 0; index < clients.length; index++) {
    const after = final[index], before = initial[index];
    if (after.frames < before.frames + seconds * 5 || after.time < before.time + seconds * 0.5 || after.error || after.dropped !== before.dropped || after.integrityErrors) {
      throw new Error(`decode did not sustain ${name}/${clients[index].kind}: ${JSON.stringify({ before, after })}`);
    }
  }
  report.phases.push({ name, seconds, initial, final });
  save(name);
}
try {
  for (const kind of ['native-js', 'native-no-js', 'peertube']) {
    const peer = kind === 'peertube';
    const context = await browser.newContext({ ignoreHTTPSErrors: true, javaScriptEnabled: kind !== 'native-no-js',
      viewport: { width: peer ? 800 : 320, height: 800 } });
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', error => errors.push(String(error)));
    if (peer) {
      await page.goto(new URL('/login', manifest.remote).href);
      await page.locator('#username').fill(process.env.PEERTUBE_E2E_USER);
      await page.locator('#password').fill(process.env.PEERTUBE_E2E_PASSWORD);
      await page.locator('input[type="submit"][value="Login"]').click();
      await page.waitForURL(url => url.pathname !== '/login');
    }
    await page.goto(peer ? manifest.remote : manifest.local, { waitUntil: 'domcontentloaded' });
    if (peer) {
      const consent = page.getByRole('button', { name: 'OK', exact: true });
      await consent.waitFor({ state: 'visible', timeout: 3000 }).catch(() => {});
      if (await consent.isVisible()) await consent.click();
    }
    const video = page.locator(peer ? 'video.vjs-tech' : 'video[data-hls]').first();
    await video.waitFor({ state: 'attached' });
    if (peer && await page.locator('.vjs-big-play-button').isVisible()) await page.locator('.vjs-big-play-button').click();
    await video.evaluate(v => { v.muted = true; v.play().catch(() => {}); });
    const client = { kind, page, video, context, errors };
    clients.push(client);
    const began = Date.now();
    let frame;
    while (Date.now() - began < 20000) {
      frame = await sample(client);
      if (!frame.paused && frame.frames >= 8 && frame.time > 0.2 && !frame.error) break;
      await pause(100);
    }
    if (frame.paused || frame.frames < 8 || frame.error) throw new Error(`initial decode failed ${kind}`);
  }
  // Record, but separate, initial audio/decoder settling from measured fault
  // phases. This is not a startup-latency workload and no frames are invented.
  report.warmupSeconds = 8;
  report.beforeWarmup = await Promise.all(clients.map(sample));
  await pause(report.warmupSeconds * 1000);
  report.afterWarmup = await Promise.all(clients.map(sample));
  await phase('healthy', 6);
  save('ready-for-fault');
  const deadline = Date.now() + 20000;
  while (!existsSync(join(evidence, 'fault-active.json'))) {
    if (Date.now() > deadline) throw new Error('owned fault was not activated');
    await pause(100);
  }
  report.fault = JSON.parse(readFileSync(join(evidence, 'fault-active.json'), 'utf8'));
  await phase('primary-fault', 24);
  save('ready-for-repair');
  const repaired = Date.now() + 20000;
  while (!existsSync(join(evidence, 'fault-repaired.json'))) {
    if (Date.now() > repaired) throw new Error('owned primary was not repaired');
    await pause(100);
  }
  await phase('repaired', 8);
  for (const client of clients) {
    const overflow = await client.page.evaluate(() => document.documentElement.scrollWidth > innerWidth);
    if (overflow || client.errors.length) throw new Error(`reflow/script failure ${client.kind}`);
    await client.page.screenshot({ path: join(evidence, `disk-${client.kind}-320px.png`) });
  }
  report.status = 'passed';
  save('complete');
} catch (error) {
  report.status = 'failed'; report.error = String(error); save('failed'); throw error;
} finally { await browser.close(); }
