// Genuine no-JS decoder and seek during the owned missing-recording drill.
import { chromium } from 'playwright';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
const [url, evidence] = process.argv.slice(2);
if (!url || !evidence) throw new Error('expected owned rewind URL and evidence');
const browser = await chromium.launch();
try {
  const context = await browser.newContext({ ignoreHTTPSErrors: true, javaScriptEnabled: false,
    viewport: { width: 320, height: 800 } });
  const page = await context.newPage();
  await page.goto(url);
  if (!await page.getByRole('heading', { name: 'Rewind this live broadcast', exact: true }).isVisible()) throw new Error('rewind form missing');
  if (!await page.getByRole('link', { name: 'Return to the broadcast' }).isVisible()) throw new Error('return link missing');
  const video = page.locator('video.live-rewind-player');
  await video.waitFor({ state: 'visible' });
  await video.evaluate(v => { v.muted = true; v.play().catch(() => {}); });
  const snapshot = () => video.evaluate(v => ({ time: v.currentTime, duration: v.duration, frames: v.getVideoPlaybackQuality().totalVideoFrames,
    dropped: v.getVideoPlaybackQuality().droppedVideoFrames, error: v.error?.code || null, seeking: v.seeking }));
  let decoded;
  const began = Date.now();
  while (Date.now() - began < 15000) {
    decoded = await snapshot();
    if (decoded.frames >= 8 && decoded.time > 0.2 && Number.isFinite(decoded.duration)) break;
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  if (decoded.frames < 8 || decoded.time <= 0.2 || !Number.isFinite(decoded.duration) || decoded.duration <= 4 || decoded.duration > 301 || decoded.error) throw new Error(`invalid finite decoder: ${JSON.stringify(decoded)}`);
  const target = Math.min(3, decoded.duration - 1);
  await video.evaluate((v, target) => { v.currentTime = target; }, target);
  let sought;
  const seeking = Date.now();
  while (Date.now() - seeking < 5000) {
    sought = await snapshot();
    if (!sought.seeking && sought.frames >= decoded.frames + 3 && sought.time >= target + 0.05) break;
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  if (sought.seeking || sought.frames < decoded.frames + 3 || sought.time < target + 0.05 || sought.error) throw new Error(`seek did not decode: ${JSON.stringify(sought)}`);
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth > innerWidth + 1);
  if (overflow) throw new Error('rewind overflows 320px');
  await page.screenshot({ path: join(evidence, 'replica-rewind-no-js-320px.png'), fullPage: true });
  const result = { applicationJavaScript: false, decoded, target, sought, seekMs: Date.now() - seeking, overflow };
  writeFileSync(join(evidence, 'replica-rewind-browser.json'), JSON.stringify(result, null, 2), { mode: 0o600 });
  console.log(JSON.stringify(result));
  await context.close();
} finally { await browser.close(); }
