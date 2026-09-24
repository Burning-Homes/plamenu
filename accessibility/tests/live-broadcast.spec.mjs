import { expect, test } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import { spawnSync } from 'node:child_process';
import { signIn } from '../session.mjs';

const createdLives = new WeakMap();

test.afterEach(async ({ page }) => {
  const id = createdLives.get(page);
  if (!id) return;
  await page.goto(`/videos/live/${id}`);
  const csrf = await page.locator('form[action$="/rotate-key"] input[name="csrf"]').getAttribute('value');
  const replayLinks = await page.locator('section[aria-labelledby="live-session-replays"] a[href^="/web/statuses/"]').evaluateAll(
    links => links.map(link => link.getAttribute('href')),
  );
  for (const href of replayLinks) {
    const replayId = href.split('/').pop();
    const removed = await page.request.post(`/web/statuses/${replayId}/delete`, {
      form: { csrf }, maxRedirects: 0,
    });
    expect(removed.status()).toBe(303);
  }
  const deleted = await page.request.post(`/web/statuses/${id}/delete`, {
    form: { csrf },
    maxRedirects: 0,
  });
  expect(deleted.status()).toBe(303);
});

async function createLive(page, permanent, returnCredentials = false) {
  await signIn(page);
  const name = `live_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 6)}`;
  await page.goto('/videos/live/new');
  let form = page.locator('form[action="/web/videos/live"]');
  if (!await form.isVisible()) {
    await page.goto('/videos');
    const channelForm = page.locator('form[action="/web/video-channels"]');
    if (!await channelForm.isVisible()) {
      await page.locator('summary').filter({ hasText: 'Create a video channel' }).click();
    }
    await channelForm.getByLabel('Channel address').fill(name);
    await channelForm.getByLabel('Channel name').fill(`Live ${name}`);
    await channelForm.getByRole('button', { name: 'Create a video channel' }).click();
    await page.waitForURL(url => url.pathname === '/videos');
    await page.goto('/videos/live/new');
    form = page.locator('form[action="/web/videos/live"]');
  }
  await expect(form.getByRole('combobox', { name: 'Channel' })).toBeVisible();
  await form.getByLabel('Video title').fill(`Browser live ${name}`);
  await form.getByLabel('Description', { exact: true }).fill('A browser-authored live test.');
  await form.getByRole('combobox', { name: 'Visibility' }).selectOption('unlisted');
  if (permanent) await form.getByRole('checkbox').check();
  await form.getByRole('button', { name: 'Create waiting broadcast' }).click();
  await expect(page.getByRole('heading', { name: 'Manage live broadcast' })).toBeVisible();
  await expect(page.getByRole('textbox', { name: 'Stream key' })).toHaveValue(/[A-Za-z0-9_-]{43}/);
  const key = await page.getByRole('textbox', { name: 'Stream key' }).inputValue();
  const server = await page.getByRole('textbox', { name: 'Plain RTMP server (trusted network only)' }).inputValue();
  await expect(page.getByRole('textbox', { name: 'Secure HTTPS-FLV ingest' })).toHaveValue(`https://${new URL(page.url()).host}/live/ingest`);
  if (process.env.PLAMENU_RTMPS === '1') {
    await expect(page.getByRole('textbox', { name: 'RTMPS server (encrypted)' })).toHaveValue(/^rtmps:\/\//);
  }
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)).toBe(true);
  const href = await page.getByRole('link', { name: 'Manage live broadcast' }).getAttribute('href');
  const id = href.split('/').pop();
  createdLives.set(page, id);
  await page.goto(href);
  await expect(page.getByText('The stream key is hidden after creation')).toBeVisible();
  return returnCredentials ? { id, key, server } : id;
}

async function liveState(page, id) {
  return (await liveMedia(page, id)).live.state;
}

async function liveMedia(page, id) {
  const response = await page.request.get(`/api/v1/statuses/${id}`);
  expect(response.ok()).toBeTruthy();
  const status = await response.json();
  return status.media_attachments[0];
}

async function installSyntheticCapture(page) {
  // Capture is synthetic, but MediaRecorder, the authenticated WebSocket,
  // server FFmpeg, HLS and player-facing state are all real browser/network IO.
  await page.addInitScript(() => {
    async function syntheticCapture() {
      const canvas = document.createElement('canvas');
      canvas.width = 640;
      canvas.height = 360;
      const context = canvas.getContext('2d');
      let frame = 0;
      setInterval(() => {
        context.fillStyle = frame++ % 2 ? '#315fdb' : '#d44e4e';
        context.fillRect(0, 0, canvas.width, canvas.height);
      }, 100);
      const video = canvas.captureStream(15);
      const audioContext = new AudioContext();
      const oscillator = audioContext.createOscillator();
      const destination = audioContext.createMediaStreamDestination();
      oscillator.frequency.value = 440;
      oscillator.connect(destination);
      oscillator.start();
      return new MediaStream([...video.getVideoTracks(), ...destination.stream.getAudioTracks()]);
    }
    Object.defineProperty(navigator.mediaDevices, 'getUserMedia', { configurable: true, value: syntheticCapture });
    Object.defineProperty(navigator.mediaDevices, 'getDisplayMedia', { configurable: true, value: syntheticCapture });
  });
}

test('browser camera and screen publish through one stable broadcast', async ({ page }) => {
  test.skip(!['chromium-desktop', 'chromium-mobile'].includes(test.info().project.name), 'real MediaRecorder pipeline is first gated in Chromium');
  test.setTimeout(240_000);
  const id = await createLive(page, true);
  await installSyntheticCapture(page);
  await page.reload();
  const publisher = page.locator('[data-live-browser]');
  await expect(publisher).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)).toBe(true);
  const violations = (await new AxeBuilder({ page }).include('[data-live-browser]').analyze()).violations;
  expect(violations).toEqual([]);
  for (const button of ['Use camera and microphone', 'Share screen']) {
    await publisher.getByRole('button', { name: button }).click();
    await expect(publisher.getByRole('status')).toContainText('Broadcasting now', { timeout: 30_000 });
    await expect.poll(() => liveState(page, id), { timeout: 30_000 }).toBe('live');
    await expect(publisher.locator('video[data-live-preview]')).toBeVisible();
    const media = await liveMedia(page, id);
    await expect.poll(async () => {
      const master = await page.request.get(`/media/hls/${media.id}/master.m3u8`);
      if (!master.ok()) return false;
      const child = (await master.text()).split('\n').find(line => line.startsWith('/media/'));
      if (!child) return false;
      const playlist = await page.request.get(child);
      if (!playlist.ok()) return false;
      const segment = (await playlist.text()).split('\n').find(line => line.endsWith('.ts'));
      if (!segment) return false;
      const data = await page.request.get(segment);
      return data.ok() && (await data.body())[0] === 0x47;
    }, { timeout: 60_000 }).toBe(true);
    await publisher.getByRole('button', { name: 'Stop broadcasting' }).click();
    await expect(publisher.getByRole('status')).toContainText('Broadcast stopped.');
    await expect.poll(() => liveState(page, id), { timeout: 30_000 }).toBe('waiting');
  }
  await expect(page.locator('[data-live-preview]')).toBeHidden();
  await expect.poll(async () => {
    await page.reload();
    return page.getByRole('link', { name: /^Watch replay [12]$/ }).count();
  }, { timeout: 120_000 }).toBe(2);
  const replayLinks = page.getByRole('link', { name: /^Watch replay [12]$/ });
  const hrefs = await replayLinks.evaluateAll(links => links.map(link => link.getAttribute('href')));
  expect(new Set(hrefs).size).toBe(2);
  await replayLinks.first().click();
  const replay = page.locator('video').first();
  await expect(replay).toBeVisible();
  await replay.evaluate(element => { element.muted = true; element.play().catch(() => {}); });
  await expect.poll(() => replay.evaluate(element => element.videoWidth), { timeout: 30_000 }).toBe(640);
  await expect.poll(() => replay.evaluate(element => element.currentTime), { timeout: 30_000 }).toBeGreaterThan(0.1);
  expect(await replay.evaluate(element => element.error)).toBeNull();
  await page.goto(`/web/statuses/${id}`);
  const publicReplays = page.getByRole('navigation', { name: 'Session replays' });
  await expect(publicReplays.getByRole('link', { name: /^Watch replay [12]$/ })).toHaveCount(2);
  const replayA11y = (await new AxeBuilder({ page }).include('nav[aria-label="Session replays"]').analyze()).violations;
  expect(replayA11y).toEqual([]);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
});

test('browser publisher can recover after an unexpected socket close', async ({ page }) => {
  test.skip(test.info().project.name !== 'chromium-desktop');
  test.setTimeout(120_000);
  const id = await createLive(page, true);
  await installSyntheticCapture(page);
  await page.addInitScript(() => {
    const NativeWebSocket = window.WebSocket;
    window.WebSocket = class extends NativeWebSocket {
      constructor(...args) {
        super(...args);
        window.__liveTestSocket = this;
      }
    };
  });
  await page.reload();
  const publisher = page.locator('[data-live-browser]');
  await publisher.getByRole('button', { name: 'Use camera and microphone' }).click();
  await expect(publisher.getByRole('status')).toContainText('Broadcasting now');
  await expect.poll(() => liveState(page, id)).toBe('live');
  await page.evaluate(() => window.__liveTestSocket.close());
  await expect(publisher.getByRole('status')).toContainText('reconnect within one minute');
  await expect.poll(() => liveState(page, id)).toBe('interrupted');
  await expect(publisher.getByRole('button', { name: 'Use camera and microphone' })).toBeEnabled();
  await publisher.getByRole('button', { name: 'Use camera and microphone' }).click();
  await expect(publisher.getByRole('status')).toContainText('Broadcasting now');
  await expect.poll(() => liveState(page, id)).toBe('live');
  await publisher.getByRole('button', { name: 'Stop broadcasting' }).click();
  await expect(publisher.getByRole('status')).toContainText('Broadcast stopped.');
  await expect.poll(() => liveState(page, id)).toBe('waiting');
});

test('live DVR rewinds and returns to the live edge accessibly', async ({ page }) => {
  test.skip(!['chromium-desktop', 'chromium-mobile'].includes(test.info().project.name));
  test.setTimeout(150_000);
  const id = await createLive(page, true);
  await installSyntheticCapture(page);
  await page.reload();
  const publisher = page.locator('[data-live-browser]');
  await publisher.getByRole('button', { name: 'Use camera and microphone' }).click();
  await expect(publisher.getByRole('status')).toContainText('Broadcasting now');
  await expect.poll(() => liveState(page, id)).toBe('live');
  const viewer = await page.context().newPage();
  try {
    await viewer.goto(`/web/statuses/${id}`);
    const video = viewer.locator('video[data-live="live"]');
    await expect(video).toBeVisible();
    await video.evaluate(element => { element.muted = true; element.play().catch(() => {}); });
    await expect.poll(() => video.evaluate(element => element.seekable.length
      ? element.seekable.end(0) - element.seekable.start(0) : 0), { timeout: 60_000 }).toBeGreaterThan(18);
    await video.evaluate(element => {
      element.currentTime = element.seekable.end(0) - 14;
    });
    const goLive = viewer.getByRole('button', { name: 'Go live' });
    await expect(goLive).toBeVisible();
    const violations = (await new AxeBuilder({ page: viewer }).include('figure.media--video').analyze()).violations;
    expect(violations).toEqual([]);
    await goLive.focus();
    await viewer.keyboard.press('Enter');
    await expect(goLive).toBeHidden();
    await expect.poll(() => video.evaluate(element => element.seekable.end(0) - element.currentTime)).toBeLessThan(9);
    expect(await viewer.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
  } finally {
    await viewer.close();
    await publisher.getByRole('button', { name: 'Stop broadcasting' }).click();
    await expect(publisher.getByRole('status')).toContainText('Broadcast stopped.');
  }
});

test('one-shot browser broadcast becomes a playable replay', async ({ page }) => {
  test.skip(!['chromium-desktop', 'chromium-mobile'].includes(test.info().project.name));
  test.setTimeout(180_000);
  const id = await createLive(page, false);
  await installSyntheticCapture(page);
  await page.reload();
  const publisher = page.locator('[data-live-browser]');
  await publisher.getByRole('button', { name: 'Use camera and microphone' }).click();
  await expect(publisher.getByRole('status')).toContainText('Broadcasting now');
  await expect.poll(() => liveState(page, id), { timeout: 30_000 }).toBe('live');
  await page.waitForTimeout(4_000);
  await publisher.getByRole('button', { name: 'Stop broadcasting' }).click();
  await expect(publisher.getByRole('status')).toContainText('Broadcast stopped.');
  await expect.poll(async () => {
    const media = await liveMedia(page, id);
    return !media.live && media.url?.endsWith('.mp4');
  }, { timeout: 90_000 }).toBe(true);
  await page.reload();
  await expect(page.getByRole('link', { name: 'Watch replay' })).toBeVisible();
  await page.getByRole('link', { name: 'Watch replay' }).click();
  const video = page.locator('video').first();
  await expect(video).toBeVisible();
  await video.evaluate(element => { element.muted = true; element.play().catch(() => {}); });
  await expect.poll(() => video.evaluate(element => element.videoWidth), { timeout: 30_000 }).toBe(640);
  await expect.poll(() => video.evaluate(element => element.currentTime), { timeout: 30_000 }).toBeGreaterThan(0.1);
  expect(await video.evaluate(element => element.error)).toBeNull();
  const violations = (await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze()).violations;
  expect(violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) }))).toEqual([]);
  const reflow = await page.evaluate(() => ({
    width: innerWidth,
    scrollWidth: document.documentElement.scrollWidth,
    overflow: [...document.querySelectorAll('body *')]
      .filter(element => element.getBoundingClientRect().right > innerWidth + 1)
      .slice(-20)
      .map(element => ({ tag: element.tagName, type: element.getAttribute('type'), className: String(element.className).slice(0, 90),
        right: Math.round(element.getBoundingClientRect().right) })),
  }));
  expect(reflow.scrollWidth, JSON.stringify(reflow)).toBeLessThanOrEqual(reflow.width + 1);
});

test('@no-js live authoring retains external encoder routes', async ({ page }) => {
  test.skip(test.info().project.name !== 'chromium-no-javascript');
  await page.setViewportSize({ width: 320, height: 800 });
  const id = await createLive(page, false);
  await expect(page.locator('[data-live-browser]')).toBeHidden();
  const playback = page.locator(`form[action="/web/videos/live/${id}/playback"]`);
  await expect(playback.getByRole('combobox', { name: 'Rewind window' })).toHaveValue('10');
  await playback.getByRole('combobox', { name: 'Rewind window' }).selectOption('5');
  await playback.getByRole('combobox', { name: 'Playback latency' }).selectOption('low');
  await playback.getByRole('button', { name: 'Save playback settings' }).click();
  await expect(playback.getByRole('combobox', { name: 'Rewind window' })).toHaveValue('5');
  expect((await liveMedia(page, id)).live).toMatchObject({ dvr_window_seconds: 300, latency_mode: 3 });
  const schedule = page.locator(`form[action="/web/videos/live/${id}/schedule"]`);
  const planned = new Date(Date.now() + 2 * 60 * 60 * 1000).toISOString().slice(0, 16);
  await schedule.getByLabel('Planned start (optional)').fill(planned);
  await schedule.getByRole('button', { name: 'Save planned start' }).click();
  await expect(schedule.getByLabel('Planned start (optional)')).toHaveValue(planned);
  expect((await liveMedia(page, id)).live.scheduled_at).toBeTruthy();
  await page.goto(`/web/statuses/${id}`);
  await expect(page.getByText('Planned start:', { exact: false })).toBeVisible();
  await expect(page.locator('figure.media--live-offline time[datetime]')).toHaveCount(1);
  await page.goto(`/videos/live/${id}`);
  await schedule.getByLabel('Planned start (optional)').fill('');
  await schedule.getByRole('button', { name: 'Save planned start' }).click();
  expect((await liveMedia(page, id)).live.scheduled_at).toBeUndefined();
  const csrf = await schedule.locator('input[name="csrf"]').inputValue();
  const invalid = await page.request.post(`/web/videos/live/${id}/schedule`, {
    form: { csrf, scheduled_at: '2001-01-01T00:00' },
  });
  expect(invalid.status()).toBe(422);
  expect((await liveMedia(page, id)).live.scheduled_at).toBeUndefined();
  await expect(page.getByRole('button', { name: 'Rotate stream key' })).toBeVisible();
  await page.goto('/videos/live/new');
  await expect(page.getByText('Each reusable broadcast session publishes a separate replay Video', { exact: false })).toBeVisible();
  expect(await liveState(page, id)).toBe('waiting');
});

test('@no-js recurring encoder publishes a separately navigable replay', async ({ page }) => {
  test.skip(test.info().project.name !== 'chromium-no-javascript');
  test.setTimeout(180_000);
  await page.setViewportSize({ width: 320, height: 800 });
  const { id, key, server } = await createLive(page, true, true);
  const sent = spawnSync('ffmpeg', [
    '-hide_banner', '-loglevel', 'error', '-f', 'lavfi', '-i', 'testsrc2=size=640x360:rate=15',
    '-f', 'lavfi', '-i', 'sine=frequency=440', '-t', '5', '-c:v', 'libx264',
    '-preset', 'veryfast', '-pix_fmt', 'yuv420p', '-c:a', 'aac', '-f', 'flv', `${server}/${key}`,
  ], { encoding: 'utf8', timeout: 30_000 });
  expect(sent.status, sent.stderr).toBe(0);
  await expect.poll(() => liveState(page, id), { timeout: 30_000 }).toBe('waiting');
  await expect.poll(async () => {
    await page.reload();
    return page.getByRole('link', { name: 'Watch replay 1' }).count();
  }, { timeout: 90_000 }).toBe(1);
  await page.getByRole('link', { name: 'Watch replay 1' }).press('Enter');
  const player = page.locator('video').first();
  await expect(player).toBeVisible();
  await expect(player).toHaveAttribute('controls', '');
  const source = await player.getAttribute('src');
  expect(source).toMatch(/\.mp4$/);
  const range = await page.request.get(source, { headers: { Range: 'bytes=0-63' } });
  expect(range.status()).toBe(206);
  await expect(page.getByText(/Replay 1/).first()).toBeVisible();
  const width = await page.evaluate(() => document.documentElement.scrollWidth);
  expect(width).toBeLessThanOrEqual(321);
  await page.goto(`/web/statuses/${id}`);
  await expect(page.getByRole('navigation', { name: 'Session replays' })
    .getByRole('link', { name: 'Watch replay 1' })).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(321);
});

test('@no-js external encoder replay stays accessible in native video controls', async ({ page }) => {
  test.skip(test.info().project.name !== 'chromium-no-javascript');
  test.setTimeout(120_000);
  await page.setViewportSize({ width: 320, height: 800 });
  const { id, key, server } = await createLive(page, false, true);
  const sent = spawnSync('ffmpeg', [
    '-hide_banner', '-loglevel', 'error', '-f', 'lavfi', '-i', 'testsrc2=size=640x360:rate=15',
    '-f', 'lavfi', '-i', 'sine=frequency=440', '-t', '5', '-c:v', 'libx264',
    '-preset', 'veryfast', '-pix_fmt', 'yuv420p', '-c:a', 'aac', '-f', 'flv', `${server}/${key}`,
  ], { encoding: 'utf8', timeout: 30_000 });
  expect(sent.status, sent.stderr).toBe(0);
  await expect.poll(async () => {
    const media = await liveMedia(page, id);
    return !media.live && media.url?.endsWith('.mp4');
  }, { timeout: 90_000 }).toBe(true);
  await page.reload();
  await page.getByRole('link', { name: 'Watch replay' }).press('Enter');
  const player = page.locator('video').first();
  await expect(player).toBeVisible();
  await expect(player).toHaveAttribute('controls', '');
  await expect(player).toHaveAttribute('src', /\.mp4$/);
  const range = await page.request.get(await player.getAttribute('src'), { headers: { Range: 'bytes=0-63' } });
  expect(range.status()).toBe(206);
  const reflow = await page.evaluate(() => {
    const scrollWidth = document.documentElement.scrollWidth;
    const mainRight = document.querySelector('.app-main').getBoundingClientRect().right;
    window.scrollTo({ left: 1000, top: 0 });
    return { scrollX, scrollWidth, mainRight };
  });
  // Allow one pixel for browser rounding; the page and composer must not pan
  // beyond the narrow viewport when scripts are disabled.
  expect(reflow.scrollWidth, JSON.stringify(reflow)).toBeLessThanOrEqual(321);
  expect(reflow.scrollX, JSON.stringify(reflow)).toBeLessThanOrEqual(1);
  expect(reflow.mainRight).toBeLessThanOrEqual(321);
});
