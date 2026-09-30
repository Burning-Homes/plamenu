// Real native decoders during the owned two-disk fixture's missing-segment
// drill. No routed media, fabricated frames or application-JS no-JS fallback.
import { chromium } from 'playwright';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
const [watchUrl, evidence] = process.argv.slice(2);
if (!watchUrl || !evidence) throw new Error('expected owned watch URL and evidence directory');
const browser = await chromium.launch();
const results = [];
try {
  for (const enabled of [true, false]) {
    const context = await browser.newContext({ ignoreHTTPSErrors: true,
      javaScriptEnabled: enabled, viewport: { width: 320, height: 700 } });
    try {
      const page = await context.newPage();
      const responses = [];
      page.on('response', response => {
        if (/\/media\//.test(response.url())) responses.push({ url: response.url(), status: response.status() });
      });
      await page.goto(watchUrl);
      const video = page.locator('video').first();
      await video.waitFor({ state: 'visible' });
      // Muted native play is harness instrumentation, not an app polyfill.
      await video.evaluate(v => { v.muted = true; v.play().catch(() => {}); });
      let first, last;
      const began = Date.now();
      while (Date.now() - began < 25000) {
        last = await video.evaluate(v => ({ time: v.currentTime, paused: v.paused,
          ready: v.readyState, frames: v.getVideoPlaybackQuality().totalVideoFrames,
          dropped: v.getVideoPlaybackQuality().droppedVideoFrames, error: v.error?.code || null }));
        if (!first && !last.paused && last.frames >= 5 && last.time > 0.1) first = { ...last, wall: Date.now() };
        if (first && Date.now() - first.wall >= 3000 && last.frames - first.frames >= 15 && last.time - first.time > 1) break;
        await new Promise(resolve => setTimeout(resolve, 100));
      }
      if (!first || last.frames - first.frames < 15 || last.time - first.time <= 1 || last.error) {
        throw new Error(`decoder did not advance (${enabled ? 'JS' : 'no-JS'}): ${JSON.stringify(last)}`);
      }
      const overflow = await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth + 1);
      if (overflow) throw new Error('live watch overflows at 320px');
      await page.screenshot({ path: join(evidence, `checkpoint-${enabled ? 'js' : 'no-js'}-320px.png`), fullPage: true });
      results.push({ javaScriptEnabled: enabled, first, last, responses, overflow });
    } finally { await context.close(); }
  }
  writeFileSync(join(evidence, 'live-browser.json'), JSON.stringify(results, null, 2), { mode: 0o600 });
  console.log(JSON.stringify(results));
} finally { await browser.close(); }
