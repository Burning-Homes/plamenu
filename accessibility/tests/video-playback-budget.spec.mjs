// Local decoded-playback budgets, not WAN/device or production capacity.
import { test, expect } from '@playwright/test';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import { signIn, openVideoOptions, createVideoChannel ,videoChannelForm} from '../session.mjs';

let directory, movie;
test.beforeAll(() => {
  directory = mkdtempSync(join(tmpdir(), 'plamenu-decoded-budget-'));
  movie = join(directory, 'moving-tone.mp4');
  execFileSync('ffmpeg', ['-nostdin', '-v', 'error', '-f', 'lavfi', '-i', 'testsrc2=size=640x480:rate=10',
    '-f', 'lavfi', '-i', 'sine=frequency=440', '-t', '24', '-c:v', 'libx264', '-threads', '2',
    '-pix_fmt', 'yuv420p', '-c:a', 'aac', '-movflags', '+faststart', movie], { timeout: 60000 });
});
test.afterAll(() => { if (directory) rmSync(directory, { recursive: true }); });

async function publish(page, noScript) {
  await signIn(page);
  await page.goto('/videos/upload');await openVideoOptions(page);
  const name = `budget_${randomUUID().replaceAll('-', '').slice(0, 18)}`;
  const channel = await videoChannelForm(page);
  if (!await channel.isVisible()) await channel.locator('..').locator(':scope > summary').press('Enter');
  await channel.getByLabel('Channel address').fill(name);
  await channel.getByLabel('Channel name').fill(name);
  await createVideoChannel(page, channel.getByRole('button', { name: 'Create a video channel', exact: true }));
  await page.waitForURL(url => url.pathname === '/videos/upload');await openVideoOptions(page);
  const upload = page.locator('form[action="/web/videos"]');
  await upload.getByRole('combobox', { name: 'Channel', exact: true }).selectOption({ label: name });
  await upload.getByLabel('Video title', { exact: true }).fill(`Decoded budget ${name}`);
  await upload.getByLabel('Visual description', { exact: true }).fill('Moving pattern');
  await upload.getByLabel('Transcript', { exact: true }).fill('Steady tone');
  await upload.getByLabel('Video or audio file', { exact: true }).setInputFiles(movie);
  await upload.getByRole('button', { name: 'Upload video', exact: true }).press('Enter');
  await page.waitForURL(url => /\/videos\/drafts\/\d+$/.test(url.pathname));
  if (noScript) {
    for (let attempt = 0; attempt < 60; attempt++) {
      if (await page.getByRole('link', { name: 'Review your video', exact: true }).isVisible()) break;
      await page.waitForTimeout(1000);
      await page.getByRole('link', { name: 'Refresh status', exact: true }).press('Enter');
    }
  }
  await expect(page.getByRole('link', { name: 'Review your video', exact: true })).toBeVisible({ timeout: 60000 });
  await page.getByRole('link', { name: 'Review your video', exact: true }).press('Enter');
  await page.getByRole('button', { name: 'Publish video', exact: true }).press('Enter');
  await page.waitForURL(url => /\/@[^/]+\/\d+/.test(url.pathname));
  return page.url();
}

async function sampledNoScript(video, page) {
  const snapshot = () => video.evaluate(v => {
    const quality = v.getVideoPlaybackQuality();
    return { now: performance.now(), time: v.currentTime, seeking: v.seeking, ready: v.readyState,
      paused: v.paused, frames: quality.totalVideoFrames - quality.droppedVideoFrames,
      quality: { totalVideoFrames: quality.totalVideoFrames, droppedVideoFrames: quality.droppedVideoFrames },
      dimensions: [v.videoWidth, v.videoHeight], error: v.error?.message || null, source: v.currentSrc };
  });
  const requested = await video.evaluate(v => { v.muted = true; v.play().catch(() => {}); return performance.now(); });
  let current = await snapshot();
  while (!(current.time > 0.1 && current.frames > 0 && !current.paused)) {
    if (current.now - requested > 15000) throw new Error(`no-JS startup: ${JSON.stringify(current)}`);
    await page.waitForTimeout(50);
    current = await snapshot();
  }
  const first = current, steadyStart = current.now;
  let steadyWaitMs = 0, longestSteadyWaitMs = 0, waiting = 0, maxSampleGapMs = 0, samples = 1;
  while (current.now - steadyStart < 8000) {
    const previous = current;
    await page.waitForTimeout(50);
    current = await snapshot();
    const gap = current.now - previous.now;
    maxSampleGapMs = Math.max(maxSampleGapMs, gap);
    samples++;
    if (previous.ready < 3) { steadyWaitMs += gap; waiting += gap; }
    else waiting = 0;
    longestSteadyWaitMs = Math.max(longestSteadyWaitMs, waiting);
    expect(current.error).toBeNull();
  }
  const last = current;
  const seek = await video.evaluate(v => { const now = performance.now(); v.currentTime = 16; v.play().catch(() => {}); return now; });
  current = await snapshot();
  // Three newly presented frames exclude an already queued pre-seek frame.
  while (!(current.time >= 16.1 && current.time < 18 && !current.seeking && current.frames >= last.frames + 3)) {
    if (current.now - seek > 10000) throw new Error(`no-JS seek: ${JSON.stringify({ last, current })}`);
    await page.waitForTimeout(50);
    current = await snapshot();
  }
  return { method: 'external snapshots; application scripts disabled', startupMs: first.now - requested,
    seekMs: current.now - seek, steadyMs: last.now - steadyStart,
    presentedSteadyFrames: last.frames - first.frames, totalPresentedFrames: current.frames,
    steadyWaitMs, longestSteadyWaitMs, maxSampleGapMs, samples, dimensions: current.dimensions,
    finalTime: current.time, error: current.error, source: current.source, quality: current.quality };
}

async function measure(page, info, lane, noScript) {
  const video = page.locator('video').first();
  await video.waitFor({ state: 'visible' });
  let result;
  if (noScript) result = await sampledNoScript(video, page);
  else {
  // Observes presented frames, not fake HLS, counters or media. No-JS uses
  // external snapshots because page callbacks are disabled by the browser.
  await video.evaluate(element => {
    if (!element.requestVideoFrameCallback) throw new Error('supported runtime lacks presented-frame measurement');
    const state = element.__budget = { requested: performance.now(), first: null, seek: null,
      seekFrame: null, waiting: [], waitingSince: null, steady: null, steadyEnd: null, presented: 0 };
    element.addEventListener('waiting', () => { if (state.waitingSince === null) state.waitingSince = performance.now(); });
    element.addEventListener('playing', () => {
      if (state.waitingSince !== null) state.waiting.push([state.waitingSince, performance.now()]);
      state.waitingSince = null;
    });
    const frame = (_now, metadata) => {
      const now = performance.now();
      state.presented++;
      if (state.first === null && !element.paused && metadata.mediaTime > 0.1) state.first = now;
      if (state.seek !== null && state.seekFrame === null && metadata.mediaTime >= 15.8 && metadata.mediaTime < 18) {
        state.seekFrame = now;
      }
      element.requestVideoFrameCallback(frame);
    };
    element.requestVideoFrameCallback(frame);
    element.muted = true;
    element.play().catch(() => {}); // Enhanced HLS intentionally cancels the initial progressive play.
  });
  await expect.poll(() => video.evaluate(v => v.__budget.first), { timeout: 15000 }).not.toBeNull();
  await video.evaluate(v => { v.__budget.steady = performance.now(); v.__budget.framesBeforeSteady = v.__budget.presented; });
  await page.waitForTimeout(8000);
  await video.evaluate(v => {
    v.__budget.steadyEnd = performance.now();
    v.__budget.framesAfterSteady = v.__budget.presented;
    v.__budget.steadyError = v.error?.message || null;
    v.__budget.seek = performance.now();
    v.currentTime = 16;
    v.play().catch(() => {});
  });
  await expect.poll(() => video.evaluate(v => v.__budget.seekFrame), { timeout: 10000 }).not.toBeNull();
  result = await video.evaluate(v => {
    const s = v.__budget;
    const waits = [...s.waiting];
    if (s.waitingSince !== null) waits.push([s.waitingSince, performance.now()]);
    const steadyWaits = waits.map(([a, b]) => Math.max(0, Math.min(b, s.steadyEnd) - Math.max(a, s.steady))).filter(ms => ms > 0);
    return { method: 'presented-frame callback and waiting/playing events', startupMs: s.first - s.requested, seekMs: s.seekFrame - s.seek,
      steadyMs: s.steadyEnd - s.steady, presentedSteadyFrames: s.framesAfterSteady - s.framesBeforeSteady,
      totalPresentedFrames: s.presented, steadyWaitMs: steadyWaits.reduce((a, b) => a + b, 0),
      longestSteadyWaitMs: Math.max(0, ...steadyWaits), dimensions: [v.videoWidth, v.videoHeight],
      finalTime: v.currentTime, error: v.error?.message || s.steadyError, source: v.currentSrc,
      quality: v.getVideoPlaybackQuality().toJSON?.() || {
        totalVideoFrames: v.getVideoPlaybackQuality().totalVideoFrames,
        droppedVideoFrames: v.getVideoPlaybackQuality().droppedVideoFrames },
    };
  });
  }
  const path = info.outputPath(`${lane}-decoded-budget.json`);
  writeFileSync(path, JSON.stringify(result, null, 2), { mode: 0o600 });
  await info.attach(`${lane}-decoded-budget.json`, { path, contentType: 'application/json' });
  await video.evaluate(v => v.pause());
  expect(result.startupMs).toBeLessThanOrEqual(10000);
  expect(result.seekMs).toBeLessThanOrEqual(5000);
  expect(result.steadyMs).toBeGreaterThanOrEqual(8000);
  expect(result.presentedSteadyFrames).toBeGreaterThanOrEqual(30);
  expect(result.steadyWaitMs).toBeLessThanOrEqual(2000);
  expect(result.longestSteadyWaitMs).toBeLessThanOrEqual(2000);
  expect(result.dimensions[0]).toBeGreaterThan(0);
  expect(result.error).toBeNull();
  if (noScript) expect(result.maxSampleGapMs).toBeLessThanOrEqual(250);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
  await page.screenshot({ path: info.outputPath(`${lane}-decoded-320px.png`), fullPage: true });
}

async function journey(page, info, noScript) {
  test.setTimeout(240000);
  const sockets = [];
  let delayedFragments = 0;
  // The real client attempts signaling; Playwright intercepts and closes its
  // WebSocket attempts. This is controlled browser fault injection (not a
  // wire outage or changed P2P preference), with genuine HTTP/decoder playback.
  if (!noScript) {
    await page.context().routeWebSocket(/\/tracker\/socket(?:\?.*)?$/, socket => { sockets.push(socket.url()); socket.close(); });
    // A tiny local film can arrive entirely by HTTP before ICE gathering ever
    // attempts a tracker connection. Keep the origin available, but delay
    // only media (not initialization) long enough to exercise actual signaling.
    await page.route('**/media/*.m4s*', async route => {
      if (!route.request().headers().range?.startsWith('bytes=0-')) {
        delayedFragments++;
        await new Promise(resolve => setTimeout(resolve, 1500));
      }
      await route.continue();
    });
  }
  const watch = await publish(page, noScript);
  await page.setViewportSize({ width: 320, height: 800 });
  sockets.length = 0;
  await page.goto(watch);
  await measure(page, info, noScript ? 'no-js-progressive' : 'tracker-unavailable-watch', noScript);
  const p2pState = await page.locator('video').first().evaluate(v => ({
    active: v.hasAttribute('data-p2p-active'), source: v.currentSrc, capability: typeof RTCPeerConnection === 'function' }));
  if (!noScript && p2pState.active) {
    expect(delayedFragments).toBeGreaterThan(0);
    expect(sockets.length).toBeGreaterThan(0);
  }
  const id = new URL(watch).pathname.split('/').pop();
  const beforeEmbed = sockets.length;
  if (!noScript) await page.unroute('**/media/*.m4s*');
  await page.goto(`/web/statuses/${id}/embed?p2p=0`);
  await measure(page, info, noScript ? 'no-js-server-only-embed' : 'server-only-embed', noScript);
  expect(sockets.length).toBe(beforeEmbed); // Explicit opt-out never contacts a tracker.
  writeFileSync(info.outputPath('signaling-fault.json'), JSON.stringify({ sockets, p2pState, delayedFragments, noScript }, null, 2), { mode: 0o600 });
}
test('decoded startup, seek and sustained playback fit local budgets after tracker failure', async ({ page }, info) => journey(page, info, false));
test('decoded startup, seek and sustained playback fit local budgets @no-js', async ({ page }, info) => journey(page, info, true));
