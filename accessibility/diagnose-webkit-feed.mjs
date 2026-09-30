// Read-only accumulated-feed regression probe. Use the pinned accessibility
// image: host WebKit additionally needs a working GStreamer audio sink.
// Credentials, cookies and HTML stay in memory; only aggregate results persist.
import { request, webkit } from 'playwright';
import { expect } from '@playwright/test';
import fs from 'node:fs';
import path from 'node:path';

const baseURL = process.env.A11Y_BASE_URL || 'https://plamenu.local';
const http = await request.newContext({ baseURL, ignoreHTTPSErrors: true });
const signed = await http.post('/login', { form: {
  identifier: process.env.A11Y_USERNAME || 'developer',
  password: process.env.A11Y_PASSWORD || 'plamenu-development-only',
}, maxRedirects: 0 });
if (signed.status() !== 303) throw new Error(`login refused: ${signed.status()}`);
const storageState = await http.storageState();
await http.dispose();
const observations = [];
for (const javaScriptEnabled of [true, false]) {
  const browser = await webkit.launch();
  const context = await browser.newContext({ ignoreHTTPSErrors: true, storageState,
    javaScriptEnabled, viewport: { width: 320, height: 800 } });
  const page = await context.newPage();
  const record = { javaScriptEnabled, crashed: false };
  page.on('crash', () => { record.crashed = true; });
  try {
    await page.goto(baseURL, { waitUntil: 'domcontentloaded', timeout: 20000 });
    const video = page.locator('video').first();
    await video.scrollIntoViewIfNeeded();
    await video.evaluate(element => { element.muted = true; element.play().catch(() => {}); });
    // Enhancement can replace the source while a native play is pending.
    // Retry the user's play action, not the navigation or browser process.
    await expect.poll(() => video.evaluate(element => {
      if (element.paused) element.play().catch(() => {});
      return element.currentTime > 0 && element.videoWidth > 0;
    }), { timeout: 20000 }).toBe(true);
    record.rendered = await page.evaluate(() => {
      const video = document.querySelector('video');
      return { videos: document.querySelectorAll('video').length,
        decodedWidth: video.videoWidth, decodedHeight: video.videoHeight,
        advanced: video.currentTime > 0,
        horizontalOverflow: document.documentElement.scrollWidth > innerWidth + 1 };
    });
  } catch (error) { record.error = error.message; }
  observations.push(record);
  await context.close();
  await browser.close();
}
const output = path.resolve(process.env.A11Y_REPORT_PATH ||
  '../target/accessibility/ap09-m7-live-termination-20260930/webkit-feed-supported.json');
fs.mkdirSync(path.dirname(output), { recursive: true });
fs.writeFileSync(output, JSON.stringify(observations, null, 2), { mode: 0o600 });
console.log(JSON.stringify(observations));
if (observations.some(value => value.crashed || value.error || !value.rendered?.advanced ||
  value.rendered.horizontalOverflow)) process.exitCode = 1;
