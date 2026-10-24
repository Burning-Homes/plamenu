import { expect, request, test } from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import { spawn, spawnSync } from 'node:child_process';
import { createConnection } from 'node:net';
import { signIn, openVideoOptions, createVideoChannel ,videoChannelForm,openDetailsFor} from '../session.mjs';

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

async function createLive(page, permanent, returnCredentials = false, visibility = 'unlisted', firstPage = false) {
  await signIn(page);
  const name = `live_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 6)}`;
  await page.goto('/videos/live/new');
  let form = page.locator('form[action="/web/videos/live"]');
  if (!await form.isVisible()) {

    const channelForm = await videoChannelForm(page);
    if (!await channelForm.isVisible()) {
      await page.locator('summary').filter({ hasText: 'Create a video channel' }).click();
    }
    await channelForm.getByLabel('Channel address').fill(name);
    await channelForm.getByLabel('Channel name').fill(`Live ${name}`);
    await channelForm.getByRole('button', { name: 'Create a video channel' }).click();
    await page.waitForURL(url => url.pathname === '/videos/live/new');
    await page.goto('/videos/live/new');
    form = page.locator('form[action="/web/videos/live"]');
  }
  if(await form.locator('select[name=channel]').count()) await expect(form.getByRole('combobox', { name: 'Channel' })).toBeVisible();
  else await expect(form.locator('input[name=channel]')).toHaveValue('default');
  await form.getByLabel('Video title').fill(`Browser live ${name}`);
  await openDetailsFor(form.getByLabel('Description', { exact: true }));
  await form.getByLabel('Description', { exact: true }).fill('A browser-authored live test.');
  await form.getByRole('combobox', { name: 'Visibility' }).selectOption(visibility);
  if (permanent) {
    await openDetailsFor(form.getByLabel('Reuse this broadcast for future sessions',{exact:true}));
    await form.getByRole('checkbox', {name:'Reuse this broadcast for future sessions',exact:true}).check();
  }
  await form.getByRole('button', { name: 'Continue to camera setup' }).click();
  await expect(page.getByRole('heading', { name: 'Manage live broadcast' })).toBeVisible();
  if (test.info().title.startsWith('camera preview')) {
    await expect(page.locator('[data-live-browser]')).toBeVisible();
    await page.screenshot({path:test.info().outputPath('live-ready-after-creation.png'),fullPage:true});
  }
  await openDetailsFor(page.getByRole('textbox', {name:'Stream key',includeHidden:true}));
  await expect(page.getByRole('textbox', { name: 'Stream key' })).toHaveValue(/[A-Za-z0-9_-]{43}/);
  const key = await page.getByRole('textbox', { name: 'Stream key' }).inputValue();
  const advertisedServer = await page.getByRole('textbox', { name: 'Plain RTMP server (trusted network only)' }).inputValue();
  const encoderServer = new URL(advertisedServer);
  // The pinned fixture's canonical localhost resolves to the TLS/browser
  // container, whereas native ingest runs on the isolated host-side address.
  // Do not change the displayed production endpoint or put keys in test logs.
  if (process.env.A11Y_LIVE_INGEST_HOST) encoderServer.hostname = process.env.A11Y_LIVE_INGEST_HOST;
  const server = encoderServer.toString().replace(/\/$/, '');
  await expect(page.getByRole('textbox', { name: 'Secure HTTPS-FLV ingest' })).toHaveValue(`https://${new URL(page.url()).host}/live/ingest`);
  if (process.env.PLAMENU_RTMPS === '1') {
    await expect(page.getByRole('textbox', { name: 'RTMPS server (encrypted)' })).toHaveValue(/^rtmps:\/\//);
  }
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)).toBe(true);
  const href = await page.getByRole('link', { name: 'Manage live broadcast' }).getAttribute('href');
  const id = href.split('/').pop();
  createdLives.set(page, id);
  if (firstPage) {
    await page.locator('summary').filter({hasText:/^External encoder and stream key$/}).press('Enter');
    return id;
  }
  await page.goto(href);
  await openDetailsFor(page.locator('form[action$="/rotate-key"]'));
  await expect(page.getByText('The stream key is hidden after creation')).toBeVisible();
  await page.locator('summary').filter({hasText:/^External encoder and stream key$/}).press('Enter');
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

// Runtime capability decides these publisher journeys, not an engine-name allowlist.
async function requireBrowserPublisher(page) {
  const recorder = await page.evaluate(() => typeof MediaRecorder === 'function');
  test.skip(!recorder, 'This browser runtime has no MediaRecorder. External encoder ingest, native replay, and player controls remain covered.');
}

async function installSyntheticCapture(page) {
  // Capture is synthetic, but MediaRecorder, the authenticated WebSocket,
  // server FFmpeg, HLS and player-facing state are all real browser/network IO.
  await page.addInitScript(() => {
    async function syntheticCapture(withAudio = true, kind = 'camera') {
      const canvas = document.createElement('canvas');
      canvas.width = 640;
      canvas.height = 360;
      const context = canvas.getContext('2d');
      let frame = 0;
      setInterval(() => {
        context.fillStyle = kind === 'screen' ? '#11bb55' : frame++ % 2 ? '#315fdb' : '#d44e4e';
        context.fillRect(0, 0, canvas.width, canvas.height);
      }, 100);
      const video = canvas.captureStream(15);
      if (!withAudio) {
        window.__lastLiveStream = video;
        (window.__liveCaptureStreams ||= []).push(video);
        return video;
      }
      const audioContext = new AudioContext();
      const oscillator = audioContext.createOscillator();
      const destination = audioContext.createMediaStreamDestination();
      oscillator.frequency.value = 440;
      oscillator.connect(destination);
      oscillator.start();
      window.__lastLiveStream = new MediaStream([...video.getVideoTracks(), ...destination.stream.getAudioTracks()]);
      (window.__liveCaptureStreams ||= []).push(window.__lastLiveStream);
      return window.__lastLiveStream;
    }
    Object.defineProperty(navigator.mediaDevices, 'getUserMedia', {
      configurable: true,
      value: constraints => {
        window.__lastLiveCapture = { kind: 'camera', constraints };
        return syntheticCapture(true, 'camera');
      },
    });
    Object.defineProperty(navigator.mediaDevices, 'getDisplayMedia', {
      configurable: true,
      value: constraints => {
        window.__lastLiveCapture = { kind: 'screen', constraints };
        return syntheticCapture(Boolean(constraints.audio), 'screen');
      },
    });
    Object.defineProperty(navigator.mediaDevices, 'enumerateDevices', {
      configurable: true,
      value: async () => [
        { kind: 'videoinput', deviceId: 'cam-default', label: 'Front camera' },
        { kind: 'videoinput', deviceId: 'cam-studio', label: 'Studio camera' },
        { kind: 'audioinput', deviceId: 'mic-default', label: 'Laptop microphone' },
        { kind: 'audioinput', deviceId: 'mic-studio', label: 'Studio microphone' },
      ],
    });
  });
}

test('browser capture choices or encoder fallback are accessible across engines', async ({ page }) => {
  test.setTimeout(120_000);
  await createLive(page, true);
  await installSyntheticCapture(page);
  await page.reload();
  const publisher = page.locator('[data-live-browser]');
  const canRecord = await page.evaluate(() => Boolean(
    window.MediaRecorder && window.WebSocket && navigator.mediaDevices?.getUserMedia,
  ));
  if (!canRecord) {
    await expect(publisher.getByRole('status')).toContainText('Use an external RTMP encoder');
    await expect(publisher.getByRole('button', { name: 'Use camera and microphone' })).toBeDisabled();
    await expect(publisher.locator('[data-live-devices]')).toBeHidden();
    return;
  }
  if (test.info().project.use.viewport?.width===320) {
    for (const control of ['[data-live-camera]','[data-live-screen]']) {
      const bounds=await publisher.locator(control).boundingBox();
      expect(bounds.y+bounds.height).toBeLessThanOrEqual(736);
    }
  }
  await publisher.locator('summary').filter({hasText:/^Capture devices$/}).press('Enter');
  await expect(publisher.getByRole('group', { name: 'Capture devices' })).toBeVisible();
  const cameraChoice = publisher.getByRole('combobox', { name: 'Camera' });
  const microphoneChoice = publisher.getByRole('combobox', { name: 'Microphone' });
  await expect(cameraChoice).toContainText('Studio camera');
  await expect(microphoneChoice).toContainText('Studio microphone');
  await cameraChoice.selectOption('cam-studio');
  await microphoneChoice.selectOption('mic-studio');
  await expect(cameraChoice).toHaveValue('cam-studio');
  await expect(microphoneChoice).toHaveValue('mic-studio');
  await expect(publisher.getByRole('checkbox', { name: /Request screen audio/ })).toBeChecked();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
  const audit = await new AxeBuilder({ page }).include('[data-live-browser]').analyze();
  expect(audit.violations).toEqual([]);
  await page.emulateMedia({ colorScheme: 'dark' });
  await page.waitForTimeout(600);
  const darkAudit = await new AxeBuilder({ page }).include('[data-live-browser]').analyze();
  expect(darkAudit.violations).toEqual([]);
});

test('browser camera and screen publish through one stable broadcast', async ({ page }) => {
  await requireBrowserPublisher(page);
  test.setTimeout(240_000);
  const id = await createLive(page, true);
  await installSyntheticCapture(page);
  await page.addInitScript(() => {
    const NativeRecorder = window.MediaRecorder;
    window.MediaRecorder = class extends NativeRecorder {
      constructor(...args) {
        super(...args);
        window.__liveRecorder = this;
      }
    };
  });
  await page.reload();
  const publisher = page.locator('[data-live-browser]');
  await expect(publisher).toBeVisible();
  await publisher.locator('summary').filter({hasText:/^Capture devices$/}).press('Enter');
  await expect(publisher.getByRole('group', { name: 'Capture devices' })).toBeVisible();
  await expect(publisher.getByRole('combobox', { name: 'Camera' })).toContainText('Studio camera');
  await publisher.getByRole('combobox', { name: 'Camera' }).selectOption('cam-studio');
  await publisher.getByRole('combobox', { name: 'Microphone' }).selectOption('mic-studio');
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)).toBe(true);
  const violations = (await new AxeBuilder({ page }).include('[data-live-browser]').analyze()).violations;
  expect(violations).toEqual([]);
  await page.emulateMedia({ colorScheme: 'dark' });
  await page.waitForTimeout(600);
  const darkViolations = (await new AxeBuilder({ page }).include('[data-live-browser]').analyze()).violations;
  expect(darkViolations).toEqual([]);
  for (const button of ['Use camera and microphone', 'Share screen']) {
    if (button === 'Share screen') {
      await openDetailsFor(publisher.getByLabel(/Request screen audio/));
      await publisher.getByRole('checkbox', { name: /Request screen audio/ }).uncheck();
    }
    await publisher.getByRole('button', { name: button }).click();
    // Selecting capture starts the existing private preview, not publication.
    await expect(publisher.getByRole('status')).toContainText('Preview only');
    expect(await liveState(page, id)).toBe('waiting');
    await publisher.locator('[data-live-start]').press('Enter');
    await expect(publisher.getByRole('status')).toContainText('Broadcasting now', { timeout: 30_000 });
    const capture = await page.evaluate(() => window.__lastLiveCapture);
    if (button === 'Use camera and microphone') {
      expect(capture).toMatchObject({
        kind: 'camera',
        constraints: {
          video: { deviceId: { exact: 'cam-studio' } },
          audio: { deviceId: { exact: 'mic-studio' } },
        },
      });
    } else {
      expect(capture).toMatchObject({ kind: 'screen', constraints: { audio: false } });
      expect(await page.evaluate(() => ({
        capturedAudio: window.__lastLiveStream.getAudioTracks().length,
        encodedAudio: window.__liveRecorder.stream.getAudioTracks().length,
      }))).toEqual({ capturedAudio: 0, encodedAudio: 1 });
    }
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
    // Capture ends synchronously; network acknowledgement/replay finalization
    // must not keep the camera or microphone open after the creator stops.
    expect(await page.evaluate(() => window.__liveCaptureStreams.every(
      stream => stream.getTracks().every(track => track.readyState === 'ended'),
    ))).toBe(true);
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

test('browser switches camera and screen inside one uninterrupted live session', async ({ page }) => {
  await requireBrowserPublisher(page);
  test.setTimeout(180_000);
  const id = await createLive(page, true);
  await installSyntheticCapture(page);
  await page.addInitScript(() => {
    const NativeRecorder = window.MediaRecorder;
    window.__liveRecorderCount = 0;
    window.MediaRecorder = class extends NativeRecorder {
      constructor(...args) {
        super(...args);
        window.__liveRecorderCount += 1;
        window.__liveRecorder = this;
      }
    };
    const NativeSocket = window.WebSocket;
    window.__liveSocketCount = 0;
    window.WebSocket = class extends NativeSocket {
      constructor(...args) {
        super(...args);
        window.__liveSocketCount += 1;
      }
    };
  });
  await page.reload();
  const publisher = page.locator('[data-live-browser]');
  await publisher.getByRole('button', { name: 'Use camera and microphone' }).click();
  await expect(publisher.getByRole('status')).toContainText(/Preview only|Broadcasting now/);
  if (await publisher.locator('[data-live-start]').isVisible()) await publisher.locator('[data-live-start]').click();
  await expect(publisher.getByRole('status')).toContainText('Broadcasting now');
  await expect.poll(() => liveState(page, id)).toBe('live');
  await expect(publisher.getByRole('button', { name: 'Switch to screen' })).toBeEnabled();
  await expect(publisher.getByRole('button', { name: 'Camera and microphone (live)' })).toBeDisabled();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
  await openDetailsFor(publisher.getByLabel(/Request screen audio/));
  await publisher.getByRole('checkbox', { name: /Request screen audio/ }).uncheck();
  await publisher.getByRole('button', { name: 'Switch to screen' }).click();
  await expect(publisher.getByRole('status')).toContainText(/Preview only|Broadcasting now/);
  if (await publisher.locator('[data-live-start]').isVisible()) await publisher.locator('[data-live-start]').click();
  await expect(publisher.getByRole('status')).toContainText('Broadcasting now');
  await expect(publisher.getByRole('button', { name: 'Switch to camera and microphone' })).toBeEnabled();
  await expect(publisher.getByRole('button', { name: 'Screen (live)' })).toBeDisabled();
  await expect(publisher.getByRole('button', { name: 'Stop broadcasting' })).toBeFocused();
  const skipLink = await page.locator('.skip-link').evaluate(element => ({
    focused: element.matches(':focus-visible'),
    top: element.getBoundingClientRect().top,
  }));
  expect(skipLink.focused).toBe(false);
  expect(skipLink.top).toBeLessThan(0);
  if (test.info().project.name === 'chromium-mobile') {
    await page.screenshot({ path: test.info().outputPath('source-switch-mobile-viewport.png') });
  }
  expect(await page.evaluate(() => ({
    kind: window.__lastLiveCapture.kind,
    oldEnded: window.__liveCaptureStreams[0].getTracks().every(track => track.readyState === 'ended'),
    recorders: window.__liveRecorderCount,
    sockets: window.__liveSocketCount,
    recording: window.__liveRecorder.state,
  }))).toEqual({ kind: 'screen', oldEnded: true, recorders: 1, sockets: 1, recording: 'recording' });
  await expect.poll(() => page.evaluate(() => {
    const preview = document.querySelector('[data-live-preview]');
    if (!preview || !preview.videoWidth) return false;
    const sample = document.createElement('canvas');
    sample.width = sample.height = 1;
    const context = sample.getContext('2d');
    context.drawImage(preview, preview.videoWidth / 2, preview.videoHeight / 2, 1, 1, 0, 0, 1, 1);
    const [red, green] = context.getImageData(0, 0, 1, 1).data;
    return green > red * 2;
  })).toBe(true);
  await publisher.getByRole('button', { name: 'Switch to camera and microphone' }).click();
  await expect(publisher.getByRole('status')).toContainText(/Preview only|Broadcasting now/);
  if (await publisher.locator('[data-live-start]').isVisible()) await publisher.locator('[data-live-start]').click();
  await expect(publisher.getByRole('status')).toContainText('Broadcasting now');
  await expect(publisher.getByRole('button', { name: 'Switch to screen' })).toBeEnabled();
  expect(await page.evaluate(() => ({
    oldEnded: window.__liveCaptureStreams[1].getTracks().every(track => track.readyState === 'ended'),
    recorders: window.__liveRecorderCount,
    sockets: window.__liveSocketCount,
  }))).toEqual({ oldEnded: true, recorders: 1, sockets: 1 });
  await page.evaluate(() => {
    Object.defineProperty(navigator.mediaDevices, 'getDisplayMedia', {
      configurable: true,
      value: () => Promise.reject(new DOMException('Denied', 'NotAllowedError')),
    });
  });
  await publisher.getByRole('button', { name: 'Switch to screen' }).click();
  await expect(publisher.getByRole('status')).toContainText('previous source remains live');
  await expect(publisher.getByRole('button', { name: 'Switch to screen' })).toBeFocused();
  expect(await page.evaluate(() => ({
    recorders: window.__liveRecorderCount,
    sockets: window.__liveSocketCount,
    recording: window.__liveRecorder.state,
    cameraLive: window.__liveCaptureStreams[2].getVideoTracks()[0].readyState,
  }))).toEqual({ recorders: 1, sockets: 1, recording: 'recording', cameraLive: 'live' });
  await expect.poll(() => liveState(page, id)).toBe('live');
  const media = await liveMedia(page, id);
  await expect.poll(async () => {
    const master = await page.request.get(`/media/hls/${media.id}/master.m3u8`);
    if (!master.ok()) return false;
    const child = (await master.text()).split('\n').find(line => line.startsWith('/media/'));
    if (!child) return false;
    const playlist = await page.request.get(child);
    if (!playlist.ok()) return false;
    return (await playlist.text()).includes('.ts');
  }, { timeout: 60_000 }).toBe(true);
  const axe = await new AxeBuilder({ page }).include('[data-live-browser]').analyze();
  expect(axe.violations).toEqual([]);
  await publisher.getByRole('button', { name: 'Stop broadcasting' }).click();
  await expect(publisher.getByRole('status')).toContainText('Broadcast stopped.');
  await expect.poll(() => liveState(page, id)).toBe('waiting');
  await expect.poll(async () => {
    await page.reload();
    return page.getByRole('link', { name: /^Watch replay 1$/ }).count();
  }, { timeout: 60_000 }).toBe(1);
  await page.getByRole('link', { name: 'Watch replay 1' }).click();
  const replay = page.locator('video').first();
  await expect(replay).toBeVisible();
  await replay.evaluate(element => { element.muted = true; element.play().catch(() => {}); });
  await expect.poll(() => replay.evaluate(element => element.videoWidth), { timeout: 30_000 }).toBe(640);
  await expect.poll(() => replay.evaluate(element => element.currentTime), { timeout: 30_000 }).toBeGreaterThan(0.1);
  expect(await replay.evaluate(element => element.error)).toBeNull();
});

test('browser publisher can recover after an unexpected socket close', async ({ page }) => {
  await requireBrowserPublisher(page);
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
  await expect(publisher.getByRole('status')).toContainText(/Preview only|Broadcasting now/);
  if (await publisher.locator('[data-live-start]').isVisible()) await publisher.locator('[data-live-start]').click();
  await expect(publisher.getByRole('status')).toContainText('Broadcasting now');
  await expect.poll(() => liveState(page, id)).toBe('live');
  await page.evaluate(() => window.__liveTestSocket.close());
  await expect(publisher.getByRole('status')).toContainText('reconnect within one minute');
  await expect.poll(() => liveState(page, id)).toBe('interrupted');
  await expect(publisher.getByRole('button', { name: 'Use camera and microphone' })).toBeEnabled();
  await publisher.getByRole('button', { name: 'Use camera and microphone' }).click();
  await expect(publisher.getByRole('status')).toContainText(/Preview only|Broadcasting now/);
  if (await publisher.locator('[data-live-start]').isVisible()) await publisher.locator('[data-live-start]').click();
  await expect(publisher.getByRole('status')).toContainText('Broadcasting now');
  await expect.poll(() => liveState(page, id)).toBe('live');
  await publisher.getByRole('button', { name: 'Stop broadcasting' }).click();
  await expect(publisher.getByRole('status')).toContainText('Broadcast stopped.');
  await expect.poll(() => liveState(page, id)).toBe('waiting');
});

test('browser capture failures and stalled startup recover without leaking devices', async ({ page }) => {
  await requireBrowserPublisher(page);
  test.setTimeout(120_000);
  const id = await createLive(page, true);
  await installSyntheticCapture(page);
  await page.reload();
  const publisher = page.locator('[data-live-browser]');
  await page.evaluate(() => {
    const camera = navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
    const screen = navigator.mediaDevices.getDisplayMedia.bind(navigator.mediaDevices);
    window.__captureError = null;
    Object.defineProperty(navigator.mediaDevices, 'getUserMedia', {
      configurable: true,
      value: constraints => window.__captureError
        ? Promise.reject(new DOMException('Synthetic capture failure', window.__captureError))
        : camera(constraints),
    });
    Object.defineProperty(navigator.mediaDevices, 'getDisplayMedia', {
      configurable: true,
      value: constraints => window.__captureError
        ? Promise.reject(new DOMException('Synthetic capture failure', window.__captureError))
        : screen(constraints),
    });
  });
  for (const [name, expected] of [
    ['NotAllowedError', 'access was denied'],
    ['NotFoundError', 'is unavailable'],
    ['NotReadableError', 'could not be read'],
  ]) {
    await page.evaluate(error => { window.__captureError = error; }, name);
    await publisher.getByRole('button', { name: 'Use camera and microphone' }).click();
    await expect(publisher.getByRole('status')).toContainText(expected);
    await expect(publisher.getByRole('button', { name: 'Use camera and microphone' })).toBeEnabled();
    await expect(publisher.locator('video[data-live-preview]')).toBeHidden();
    expect(await liveState(page, id)).toBe('waiting');
  }
  await page.evaluate(() => { window.__captureError = 'AbortError'; });
  await publisher.getByRole('button', { name: 'Share screen' }).click();
  await expect(publisher.getByRole('status')).toContainText('Screen sharing was cancelled');
  await expect(publisher.getByRole('button', { name: 'Share screen' })).toBeEnabled();
  await page.evaluate(() => {
    window.__captureError = null;
    window.__nativeLiveSocket = WebSocket;
    class StalledSocket {
      static OPEN = 1;
      constructor() {
        this.readyState = 0;
        setTimeout(() => { this.readyState = 1; this.onopen?.(); }, 0);
      }
      send() {}
      close() { this.readyState = 3; this.onclose?.(); }
    }
    window.WebSocket = StalledSocket;
  });
  await publisher.getByRole('button', { name: 'Use camera and microphone' }).click();
  await publisher.locator('[data-live-start]').click();
  await expect(publisher.getByRole('status')).toContainText('did not respond', { timeout: 20_000 });
  await expect(publisher.getByRole('button', { name: 'Use camera and microphone' })).toBeEnabled();
  expect(await page.evaluate(() => window.__lastLiveStream.getTracks().every(track => track.readyState === 'ended'))).toBe(true);
  expect(await liveState(page, id)).toBe('waiting');
  await page.evaluate(() => { window.WebSocket = window.__nativeLiveSocket; });
  await publisher.getByRole('button', { name: 'Use camera and microphone' }).click();
  await expect(publisher.getByRole('status')).toContainText(/Preview only|Broadcasting now/);
  if (await publisher.locator('[data-live-start]').isVisible()) await publisher.locator('[data-live-start]').click();
  await expect(publisher.getByRole('status')).toContainText('Broadcasting now');
  await expect.poll(() => liveState(page, id)).toBe('live');
  await publisher.getByRole('button', { name: 'Stop broadcasting' }).click();
  await expect(publisher.getByRole('status')).toContainText('Broadcast stopped.');
  await expect.poll(() => liveState(page, id)).toBe('waiting');
  const axe = await new AxeBuilder({ page }).include('[data-live-browser]').analyze();
  expect(axe.violations).toEqual([]);
});

test('unavailable browser audio releases capture and permits a real retry', async ({ page }) => {
  await requireBrowserPublisher(page);
  const id = await createLive(page, true);
  await installSyntheticCapture(page);
  await page.reload();
  const publisher = page.locator('[data-live-browser]');
  await page.evaluate(() => {
    window.__nativeAudioContext = window.AudioContext;
    window.AudioContext = class extends window.__nativeAudioContext {
      resume() { return new Promise(() => {}); }
    };
  });
  await publisher.getByRole('button', { name: 'Use camera and microphone' }).focus();
  await page.keyboard.press('Enter');
  await expect(publisher.getByRole('status')).toContainText('audio system did not start', { timeout: 10_000 });
  await expect(publisher.getByRole('button', { name: 'Use camera and microphone' })).toBeEnabled();
  expect(await page.evaluate(() => window.__liveCaptureStreams.every(stream => stream.getTracks().every(track => track.readyState === 'ended')))).toBe(true);
  expect(await liveState(page, id)).toBe('waiting');
  await page.evaluate(() => { window.AudioContext = window.__nativeAudioContext; });
  await publisher.getByRole('button', { name: 'Use camera and microphone' }).click();
  await expect(publisher.getByRole('status')).toContainText(/Preview only|Broadcasting now/);
  if (await publisher.locator('[data-live-start]').isVisible()) await publisher.locator('[data-live-start]').click();
  await expect(publisher.getByRole('status')).toContainText('Broadcasting now');
  await expect.poll(() => liveState(page, id)).toBe('live');
  await publisher.getByRole('button', { name: 'Stop broadcasting' }).click();
  await expect(publisher.getByRole('status')).toContainText('Broadcast stopped.');
});

test('live DVR rewinds and returns to the live edge accessibly', async ({ page }, info) => {
  await requireBrowserPublisher(page);
  test.setTimeout(150_000);
  const id = await createLive(page, true);
  await installSyntheticCapture(page);
  await page.reload();
  const publisher = page.locator('[data-live-browser]');
  await publisher.getByRole('button', { name: 'Use camera and microphone' }).click();
  await expect(publisher.getByRole('status')).toContainText(/Preview only|Broadcasting now/);
  if (await publisher.locator('[data-live-start]').isVisible()) await publisher.locator('[data-live-start]').click();
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
    await expect(viewer.locator('.vod-controls')).toBeVisible();
    expect(await video.evaluate(v=>v.controls)).toBe(false);
    await expect(viewer.getByRole('slider',{name:'Seek',exact:true})).toBeVisible();
    const goLive = viewer.getByRole('button', { name: 'Go live' });
    await expect(goLive).toBeVisible();
    const controlBounds=await goLive.boundingBox(), pictureBounds=await video.boundingBox();
    expect(controlBounds.y).toBeGreaterThanOrEqual(pictureBounds.y+pictureBounds.height-1);
    expect(controlBounds.x+controlBounds.width).toBeLessThanOrEqual(pictureBounds.x+pictureBounds.width+1);
    expect(controlBounds.width).toBeGreaterThanOrEqual(44);expect(controlBounds.height).toBeGreaterThanOrEqual(44);
    await viewer.screenshot({path:info.outputPath('running-live-enhanced-player.png'),fullPage:true,animations:'disabled'});
    // Live-edge stall recovery must not undo a deliberate DVR rewind.
    await viewer.waitForTimeout(3500);
    expect(await video.evaluate(element => element.seekable.end(0) - element.currentTime)).toBeGreaterThan(10);
    const violations = (await new AxeBuilder({ page: viewer }).include('figure.media--video').analyze()).violations;
    expect(violations).toEqual([]);
    if(await viewer.evaluate(()=>document.fullscreenEnabled)) {
      await viewer.getByRole('button',{name:'Fullscreen',exact:true}).click();
      await expect.poll(()=>viewer.evaluate(()=>Boolean(document.fullscreenElement))).toBe(true);
      await viewer.mouse.move(30,30);
      await expect.poll(()=>viewer.locator('.vod-controls').evaluate(e=>getComputedStyle(e).opacity),{timeout:7000}).toBe('0');
      await viewer.screenshot({path:info.outputPath('live-fullscreen-unobstructed.png')});
      await viewer.evaluate(()=>document.exitFullscreen());
    }
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

test('fatal live HLS failure offers accessible basic and refresh paths', async ({ page }) => {
  await requireBrowserPublisher(page);
  test.setTimeout(240_000);
  const id = await createLive(page, true);
  await installSyntheticCapture(page);
  await page.reload();
  const publisher = page.locator('[data-live-browser]');
  await publisher.getByRole('button', { name: 'Use camera and microphone' }).click();
  await expect(publisher.getByRole('status')).toContainText(/Preview only|Broadcasting now/);
  if (await publisher.locator('[data-live-start]').isVisible()) await publisher.locator('[data-live-start]').click();
  await expect(publisher.getByRole('status')).toContainText('Broadcasting now');
  await expect.poll(() => liveState(page, id)).toBe('live');
  const viewer = await page.context().newPage();
  try {
    if (test.info().project.name === 'chromium-mobile') {
      await viewer.setViewportSize({ width: 320, height: 800 });
    }
    await viewer.route('**/media/hls/**', route => route.fulfill({ status: 503, body: 'Injected segment failure' }));
    await viewer.goto(`/web/statuses/${id}`);
    await viewer.evaluate(() => localStorage.setItem('plamenu:p2p-enabled', 'false'));
    await viewer.reload();
    const video = viewer.locator('video[data-live="live"]');
    await expect(video).toBeVisible();
    await video.evaluate(element => { element.muted = true; element.play().catch(() => {}); });
    const recovery = viewer.locator('figure.media--live-failed .media__live-recovery');
    await expect(recovery).toContainText('Live playback was interrupted', { timeout: 120_000 });
    await expect(recovery).toHaveAttribute('role', 'status');
    const basic = recovery.getByRole('link', { name: 'Try basic stream' });
    await expect(basic).toHaveAttribute('href', await video.getAttribute('data-src'));
    const refresh = recovery.getByRole('link', { name: 'Refresh broadcast status' });
    await expect(refresh).toHaveAttribute('href', viewer.url());
    if (test.info().project.name === 'chromium-mobile') {
      expect(await viewer.evaluate(() => {
        const barTop = document.querySelector('.tabbar').getBoundingClientRect().top;
        return [...document.querySelectorAll('.media__live-recovery a')].every(link => {
          const bounds = link.getBoundingClientRect();
          return bounds.top >= 0 && bounds.bottom < barTop;
        });
      })).toBe(true);
    }
    const audit = await new AxeBuilder({ page: viewer }).include('figure.media--live-failed').analyze();
    expect(audit.violations).toEqual([]);
    await viewer.emulateMedia({ colorScheme: 'dark' });
    await viewer.waitForTimeout(600);
    const darkAudit = await new AxeBuilder({ page: viewer }).include('figure.media--live-failed').analyze();
    expect(darkAudit.violations).toEqual([]);
    expect(await viewer.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
    await refresh.focus();
    await expect(refresh).toBeFocused();
    await viewer.unroute('**/media/hls/**');
    await refresh.click();
    const recovered = viewer.locator('video[data-live="live"]');
    await expect(recovered).toBeVisible();
    await expect(viewer.locator('.media__live-recovery')).toHaveCount(0);
    await recovered.evaluate(element => { element.muted = true; element.play().catch(() => {}); });
    await expect.poll(() => recovered.evaluate(element => element.currentTime), { timeout: 30_000 }).toBeGreaterThan(0.1);
    await expect.poll(() => recovered.evaluate(element => element.videoWidth), { timeout: 30_000 }).toBeGreaterThan(0);
    expect(await recovered.evaluate(element => element.error)).toBeNull();
    if (test.info().project.name === 'chromium-desktop') {
      let failedSegments = 0;
      await viewer.route('**/media/hls/**/segment*.ts', route => {
        failedSegments += 1;
        return route.fulfill({ status: 503, body: 'Injected segment failure' });
      });
      await viewer.reload();
      const segmentVideo = viewer.locator('video[data-live="live"]');
      await segmentVideo.evaluate(element => { element.muted = true; element.play().catch(() => {}); });
      await expect(viewer.locator('.media__live-recovery')).toBeVisible({ timeout: 90_000 });
      expect(failedSegments).toBeGreaterThan(0);
    }
  } finally {
    await viewer.close();
    await publisher.getByRole('button', { name: 'Stop broadcasting' }).click();
    await expect(publisher.getByRole('status')).toContainText('Broadcast stopped.');
  }
});

test('one-shot browser broadcast becomes a playable replay', async ({ page }) => {
  await requireBrowserPublisher(page);
  test.setTimeout(180_000);
  const id = await createLive(page, false);
  await installSyntheticCapture(page);
  await page.reload();
  const publisher = page.locator('[data-live-browser]');
  await publisher.getByRole('button', { name: 'Use camera and microphone' }).click();
  await expect(publisher.getByRole('status')).toContainText(/Preview only|Broadcasting now/);
  if (await publisher.locator('[data-live-start]').isVisible()) await publisher.locator('[data-live-start]').click();
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
  await openDetailsFor(playback);
  await expect(playback.getByRole('combobox', { name: 'Rewind window' })).toHaveValue('10');
  await playback.getByRole('combobox', { name: 'Rewind window' }).selectOption('5');
  await playback.getByRole('combobox', { name: 'Playback latency' }).selectOption('low');
  await playback.getByRole('button', { name: 'Save playback settings' }).click();
  await openDetailsFor(playback);
  await expect(playback.getByRole('combobox', { name: 'Rewind window' })).toHaveValue('5');
  expect((await liveMedia(page, id)).live).toMatchObject({ dvr_window_seconds: 300, latency_mode: 3 });
  const schedule = page.locator(`form[action="/web/videos/live/${id}/schedule"]`);
  await openDetailsFor(schedule);
  const planned = new Date(Date.now() + 2 * 60 * 60 * 1000).toISOString().slice(0, 16);
  const second = new Date(Date.now() + 4 * 60 * 60 * 1000).toISOString().slice(0, 16);
  const third = new Date(Date.now() + 6 * 60 * 60 * 1000).toISOString().slice(0, 16);
  await schedule.getByLabel('Planned start (optional)').fill(planned);
  await schedule.getByLabel('More planned starts (optional)').fill(`${third}\n${second}`);
  await schedule.getByRole('button', { name: 'Save planned start' }).click();
  await expect(schedule.getByLabel('Planned start (optional)')).toHaveValue(planned);
  await expect(schedule.getByLabel('More planned starts (optional)')).toHaveValue(`${second}\n${third}`);
  expect((await liveMedia(page, id)).live.schedules).toHaveLength(3);
  const live = (await liveMedia(page, id)).live;
  expect(live.calendar_url).toContain(`/videos/live/${id}/calendar.ics`);
  await openDetailsFor(page.getByLabel('Broadcast calendar URL',{exact:true}));
  await expect(page.getByRole('textbox', { name: 'Broadcast calendar URL' })).toHaveValue(live.calendar_url);
  const anonymous = await request.newContext({ ignoreHTTPSErrors: false });
  const calendar = await anonymous.get(live.calendar_url);
  expect(calendar.status()).toBe(200);
  expect(calendar.headers()['content-type']).toContain('text/calendar');
  const calendarText = await calendar.text();
  expect(calendarText.match(/BEGIN:VEVENT/g)).toHaveLength(3);
  expect(calendarText).toContain('STATUS:TENTATIVE\r\n');
  expect(calendarText).not.toContain('BEGIN:VALARM');
  const unchanged = await anonymous.get(live.calendar_url, {
    headers: { 'If-None-Match': calendar.headers().etag },
  });
  expect(unchanged.status()).toBe(304);
  const reminder = await anonymous.get(`${live.calendar_url}?reminder=15`);
  expect((await reminder.text()).match(/TRIGGER:-PT15M/g)).toHaveLength(3);
  const channel = await anonymous.get(live.channel_calendar_url);
  expect(channel.status()).toBe(200);
  expect(await channel.text()).not.toContain(`/web/statuses/${id}`); // this broadcast is unlisted
  await anonymous.dispose();
  await page.goto(`/web/statuses/${id}`);
  await expect(page.getByText('Planned start:', { exact: false })).toBeVisible();
  await expect(page.locator('figure.media--live-offline time[datetime]')).toHaveCount(4);
  await expect(page.getByText('Upcoming broadcasts')).toBeVisible();
  await expect(page.getByRole('link', { name: 'Add planned starts to calendar' })).toHaveAttribute('href', live.calendar_url);
  await expect(page.getByRole('link', { name: 'Add with a 15-minute reminder' })).toBeVisible();
  await expect(page.getByRole('link', { name: 'Subscribe to public channel live calendar' })).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
  await page.goto(`/videos/live/${id}`);
  const csrf = await schedule.locator('input[name="csrf"]').inputValue();
  const duplicate = await page.request.post(`/web/videos/live/${id}/schedule`, {
    form: { csrf, scheduled_at: planned, scheduled_starts: planned },
  });
  expect(duplicate.status()).toBe(422);
  expect((await liveMedia(page, id)).live.schedules).toHaveLength(3);
  await openDetailsFor(schedule);
  await schedule.getByLabel('Planned start (optional)').fill('');
  await schedule.getByLabel('More planned starts (optional)').fill('');
  await schedule.getByRole('button', { name: 'Save planned start' }).click();
  expect((await liveMedia(page, id)).live.scheduled_at).toBeUndefined();
  expect((await liveMedia(page, id)).live.schedules).toBeUndefined();
  const invalid = await page.request.post(`/web/videos/live/${id}/schedule`, {
    form: { csrf, scheduled_at: '2001-01-01T00:00' },
  });
  expect(invalid.status()).toBe(422);
  expect((await liveMedia(page, id)).live.scheduled_at).toBeUndefined();
  await openDetailsFor(page.locator('form[action$="/rotate-key"]'));
  await expect(page.getByRole('button', { name: 'Rotate stream key' })).toBeVisible();
  await page.goto('/videos/live/new');
  await openDetailsFor(page.getByText('Each reusable broadcast session publishes a separate replay Video', { exact: false }));
  await expect(page.getByText('Each reusable broadcast session publishes a separate replay Video', { exact: false })).toBeVisible();
  expect(await liveState(page, id)).toBe('waiting');
});

async function publicLiveCalendar(page, noScript) {
  await page.setViewportSize({width:320,height:800});
  const id = await createLive(page, false, false, 'public');
  const schedule = page.locator(`form[action="/web/videos/live/${id}/schedule"]`);
  await openDetailsFor(schedule);
  const planned = new Date(Date.now() + 2 * 60 * 60 * 1000).toISOString().slice(0, 16);
  await schedule.getByLabel('Planned start (optional)').fill(planned);
  await schedule.getByRole('button', { name: 'Save planned start' }).click();
  const live = (await liveMedia(page, id)).live;
  const anonymous = await request.newContext({ ignoreHTTPSErrors: false });
  const feed = await anonymous.get(live.channel_calendar_url);
  expect(feed.status()).toBe(200);
  expect(await feed.text()).toContain(`URL:https://${new URL(page.url()).host}/web/statuses/${id}`);
  const reminder=await anonymous.get(`${live.calendar_url}?reminder=15`);
  expect(reminder.status()).toBe(200);
  expect(await reminder.text()).toContain('TRIGGER:-PT15M');
  const etag=feed.headers().etag;
  expect(etag).toBeTruthy();
  const unchanged=await anonymous.get(live.channel_calendar_url,{headers:{'If-None-Match':etag}});
  expect(unchanged.status()).toBe(304);
  const invalid = await page.request.get(`${live.channel_calendar_url}?reminder=30`);
  expect(invalid.status()).toBe(400);
  await anonymous.dispose();
  await page.goto(`/web/statuses/${id}`);
  await expect(page.getByRole('link', { name: 'Subscribe to public channel live calendar' })).toBeVisible();
  await expect(page.getByRole('link',{name:'Add with a 15-minute reminder'})).toHaveAttribute('href',`${live.calendar_url}?reminder=15`);
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
  if(!noScript) {
    const result=await new AxeBuilder({page}).include('figure.media--live-offline').withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
    expect(result.violations).toEqual([]);
  }
  await page.screenshot({path:test.info().outputPath('public-live-calendar-320px.png'),fullPage:true});
}
test('public live starts and reminders appear in the channel calendar',async({page})=>publicLiveCalendar(page,false));
test('@no-js public live starts appear in the channel calendar', async ({ page }) => {
  await publicLiveCalendar(page,true);
});

test('live schedule picker adds a date accessibly before publishing', async ({ page }) => {
  test.skip(test.info().project.name.includes('no-javascript'));
  await page.setViewportSize({ width: 320, height: 800 });
  const id = await createLive(page, true);
  const schedule = page.locator(`form[action="/web/videos/live/${id}/schedule"]`);
  await openDetailsFor(schedule);
  const planned = new Date(Date.now() + 3 * 60 * 60 * 1000).toISOString().slice(0, 16);
  await expect(schedule.getByLabel('Pick another planned date and time')).toBeVisible();
  await schedule.getByLabel('Pick another planned date and time').fill(planned);
  await schedule.getByRole('button', { name: 'Add to planned starts' }).click();
  await expect(schedule.getByRole('status')).toContainText('Save the form');
  await expect(schedule.getByLabel('More planned starts (optional)')).toHaveValue(planned);
  expect((await new AxeBuilder({ page }).include('form[data-live-schedules]').analyze()).violations).toEqual([]);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
  if (test.info().project.name.endsWith('mobile')) {
    await page.screenshot({ path: test.info().outputPath(`live-schedule-${test.info().project.name}.png`), fullPage: true });
  }
  await schedule.getByRole('button', { name: 'Save planned start' }).click();
  expect((await liveMedia(page, id)).live.schedules).toHaveLength(1);
  await page.goto(`/web/statuses/${id}`);
  expect((await new AxeBuilder({ page }).include('figure.media--live-offline').analyze()).violations).toEqual([]);
  if (test.info().project.name.endsWith('mobile')) {
    await page.screenshot({ path: test.info().outputPath(`live-calendar-${test.info().project.name}.png`), fullPage: true });
  }
});

test('@no-js live rewind serves a seekable, bounded native clip', async ({ page }) => {
  test.skip(test.info().project.name !== 'chromium-no-javascript');
  test.setTimeout(120_000);
  await page.setViewportSize({ width: 320, height: 800 });
  const { id, key, server } = await createLive(page, false, true);
  const publisher = spawn('ffmpeg', [
    '-hide_banner', '-loglevel', 'error', '-f', 'lavfi', '-i', 'testsrc2=size=160x90:rate=10',
    '-f', 'lavfi', '-i', 'sine=frequency=440:sample_rate=22050',
    '-vf', 'realtime=speed=20', '-af', 'arealtime=speed=20',
    '-c:v', 'libx264', '-preset', 'ultrafast', '-g', '20', '-pix_fmt', 'yuv420p',
    '-c:a', 'aac', '-b:a', '48k', '-f', 'flv', `${server}/${key}`,
  ], { stdio: 'ignore' });
  let source;
  try {
    await expect.poll(() => liveState(page, id), { timeout: 30_000 }).toBe('live');
    // The ingest handshake precedes the first committed media segment.
    await expect.poll(async () => {
      const media = await liveMedia(page, id);
      const master = await page.request.get(`/media/hls/${media.id}/master.m3u8`);
      return master.status();
    }, { timeout: 30_000 }).toBe(200);
    await page.goto(`/web/statuses/${id}`);
    await page.getByRole('link', { name: 'Rewind this live broadcast' }).click();
    await expect(page.getByRole('heading', { name: 'Rewind this live broadcast' })).toBeVisible();
    await expect(page.getByRole('link', { name: 'Return to the broadcast' })).toBeVisible();
    const video = page.locator('video.live-rewind-player[aria-label="Selected live rewind portion"]');
    await expect(video).toBeVisible();
    await expect.poll(() => video.evaluate(element => element.duration), { timeout: 30_000 })
      .toBeGreaterThan(4);
    source = await video.getAttribute('src');
    const range = await page.request.get(source, { headers: { Range: 'bytes=0-63' } });
    expect(range.status()).toBe(206);
    expect(range.headers()['content-range']).toMatch(/^bytes 0-63\//);
    const invalidRange = await page.request.get(source, { headers: { Range: 'bytes=999999999999-' } });
    expect(invalidRange.status()).toBe(416);
    const stale = source.replace(/\/rewind\/(\d+)\//, (_all, bucket) => `/rewind/${Number(bucket) - 11}/`);
    expect((await page.request.get(stale)).status()).toBe(404);
    const extreme = source.replace(/\/rewind\/\d+\//, '/rewind/-9223372036854775808/');
    expect((await page.request.get(extreme)).status()).toBe(404);
    expect((await page.request.get(source.replace(/\/0\.mp4$/, '/99.mp4'))).status()).toBe(404);
    await video.evaluate(element => { element.currentTime = Math.min(3, element.duration - 1); });
    await expect.poll(() => video.evaluate(element => element.currentTime)).toBeGreaterThan(1);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
    await page.screenshot({ path: test.info().outputPath('live-rewind-nojs-mobile.png'), fullPage: true });
    await page.evaluate(() => window.scrollTo(0, document.documentElement.scrollHeight));
    const bottomClear = await page.evaluate(() => {
      const hint = document.querySelector('.live-rewind-player + .settings-field__hint');
      const tabbar = document.querySelector('.tabbar');
      return hint.getBoundingClientRect().bottom < tabbar.getBoundingClientRect().top;
    });
    expect(bottomClear).toBe(true);
  } finally {
    publisher.kill('SIGTERM');
    if (publisher.exitCode === null) {
      await new Promise(resolve => publisher.once('exit', resolve));
    }
  }
  await expect.poll(async () => Boolean((await liveMedia(page, id)).live), { timeout: 90_000 })
    .toBe(false);
  expect((await page.request.get(source)).status()).toBe(404);
});

test('@no-js recurring encoder publishes a separately navigable replay', async ({ page }) => {
  test.skip(test.info().project.name !== 'chromium-no-javascript');
  test.setTimeout(180_000);
  await page.setViewportSize({ width: 320, height: 800 });
  const { id, key, server } = await createLive(page, true, true);
  const endpoint = new URL(server);
  await new Promise((resolve, reject) => {
    const socket = createConnection({host:endpoint.hostname,port:Number(endpoint.port)});
    socket.setTimeout(5000);
    socket.once('connect',()=>{socket.destroy();resolve();});
    socket.once('error',error=>reject(new Error(`Native encoder fixture is unreachable: ${error.code}`)));
    socket.once('timeout',()=>{socket.destroy();reject(new Error(`Native encoder fixture timed out at ${endpoint.host}`));});
  });
  const sent = spawnSync('ffmpeg', [
    '-hide_banner', '-loglevel', 'error', '-f', 'lavfi', '-i', 'testsrc2=size=640x360:rate=15',
    '-f', 'lavfi', '-i', 'sine=frequency=440', '-t', '5', '-c:v', 'libx264',
    '-preset', 'veryfast', '-pix_fmt', 'yuv420p', '-c:a', 'aac', '-f', 'flv', `${server}/${key}`,
  ], { encoding: 'utf8', timeout: 30_000 });
  expect(sent.status, JSON.stringify({stderr:sent.stderr?.replaceAll(key,'[redacted]'),signal:sent.signal,error:sent.error?.code,endpoint:endpoint.host})).toBe(0);
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


test('camera preview stays private until Go live and cancellation releases devices', async ({page},info) => {
  await requireBrowserPublisher(page);
  test.setTimeout(90000);
  await installSyntheticCapture(page);
  await page.addInitScript(() => {
    const Socket=window.WebSocket;
    window.__previewConnections=0;
    window.WebSocket=class extends Socket { constructor(...args) {super(...args);if(String(args[0]).includes("/publish"))window.__previewConnections++;} };
  });
  const id=await createLive(page,true,false,'unlisted',true);
  const publisher=page.locator('[data-live-browser]');
  await publisher.locator('[data-live-camera]').click();
  await expect(publisher.getByRole('status')).toContainText('Preview only');
  await expect(publisher.locator('video')).toBeVisible();
  expect(await liveState(page,id)).toBe('waiting');
  expect(await page.evaluate(()=>window.__previewConnections)).toBe(0);
  await expect(publisher.locator('[data-live-start]')).toBeFocused();
  expect(await page.locator('.skip-link').evaluate(e=>e.getBoundingClientRect().bottom)).toBeLessThanOrEqual(0);
  await page.screenshot({path:info.outputPath('live-camera-viewport.png')});
  // Capture from the top: Chromium's full-page capture otherwise paints fixed,
  // off-viewport elements at their document-relative position in the PNG.
  await page.locator('h1').click(); await page.evaluate(()=>scrollTo(0,0));
  await page.screenshot({path:info.outputPath('live-camera-preview.png'),fullPage:true});
  await publisher.locator('[data-live-cancel]').click();
  await expect(publisher.locator('video')).toBeHidden();
  expect(await page.evaluate(()=>window.__liveCaptureStreams.every(stream=>stream.getTracks().every(track=>track.readyState==='ended')))).toBe(true);
  expect(await page.evaluate(()=>window.__previewConnections)).toBe(0);
});
