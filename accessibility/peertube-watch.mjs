// Invoked by the outbound PeerTube E2E while its Plamenu video still exists.
// The normal first-party browser matrix cannot prove PeerTube's own player.
import assert from 'node:assert/strict';
import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium, firefox } from 'playwright';
import { installPeerTubeWelcomeHandler } from './peertube-session.mjs';

const watchUrl = process.argv[2];
const audioOnly = process.argv[3] === 'audio-only';
if (!watchUrl || new URL(watchUrl).hostname !== 'peertube.local') {
  throw new Error('expected the local PeerTube watch URL');
}
if (!process.env.PEERTUBE_E2E_USER || !process.env.PEERTUBE_E2E_PASSWORD) {
  throw new Error('local PeerTube browser credentials are required');
}

const results = [];
// The watch matrix shares one authenticated session. Logging in for every
// viewport/engine can exhaust PeerTube's anonymous API rate limit before its
// login page has even loaded; storage state is portable across these engines.
let authenticatedState;
for (const [engineName, engine] of [['chromium', chromium], ['firefox', firefox]]) {
  const browser = await engine.launch({ headless: true });
  try {
    for (const [name, viewport, mobile] of [
      ['desktop', { width: 1440, height: 900 }, false],
      ['mobile-320', { width: 320, height: 800 }, true],
    ]) {
      const context = await browser.newContext({
        viewport, isMobile: engineName === 'chromium' && mobile,
        hasTouch: mobile, ignoreHTTPSErrors: true,
        ...(authenticatedState ? { storageState: authenticatedState } : {}),
      });
      try {
        console.error(`PeerTube watch check: ${engineName} ${name}`);
        const page = await context.newPage();
        await installPeerTubeWelcomeHandler(page);
        const errors = [];
        const mediaRequests = [];
        const badResponses = [];
        page.on('pageerror', error => errors.push(error.message));
        page.on('requestfailed', request => {
          if (request.url().includes('plamenu.local')) {
            mediaRequests.push({ url: request.url(), failure: request.failure()?.errorText });
          }
        });
        page.on('response', response => {
          if (response.status() >= 400) {
            const url = new URL(response.url());
            badResponses.push({ origin: url.origin, path: url.pathname, status: response.status() });
          }
          if (response.url().includes('plamenu.local')) {
            mediaRequests.push({ url: response.url(), status: response.status() });
          }
        });
        if (!authenticatedState) {
          await page.goto(new URL('/login', watchUrl).href, { waitUntil: 'domcontentloaded' });
          try {
            await page.locator('#username').waitFor({ state: 'visible', timeout: 15_000 });
          } catch (error) {
            console.error(JSON.stringify({
              engineName, name, phase: 'login', url: page.url(), title: await page.title(),
              body: (await page.locator('body').innerText()).slice(0, 1200),
              errors, badResponses,
            }));
            throw error;
          }
          await page.locator('#username').fill(process.env.PEERTUBE_E2E_USER);
          await page.locator('#password').fill(process.env.PEERTUBE_E2E_PASSWORD);
          await page.locator('input[type="submit"][value="Login"]').click();
          await page.waitForURL(url => url.pathname !== '/login', { timeout: 30_000 });
          authenticatedState = await context.storageState();
        }
        const navigationStart = performance.now();
        await page.goto(watchUrl, { waitUntil: 'domcontentloaded', timeout: 30_000 });
        const privacyNotice = page.getByRole('button', { name: 'OK', exact: true });
        await privacyNotice.waitFor({ state: 'visible', timeout: 10_000 }).catch(() => {});
        if (await privacyNotice.isVisible()) await privacyNotice.click();
        const player = page.locator('video.vjs-tech');
        await player.waitFor({ state: 'attached', timeout: 30_000 });
        if (await player.evaluate(video => video.paused)) {
          const bigPlay = page.locator('.vjs-big-play-button');
          if (await bigPlay.isVisible()) await bigPlay.click();
          else await page.locator('.video-js').click();
        }
        try {
          await page.waitForFunction(() => {
            const video = document.querySelector('video.vjs-tech');
            return video && video.currentTime > 0.2 && video.videoWidth > 0 && !video.error;
          }, undefined, { timeout: 30_000 });
        } catch (error) {
          console.error(JSON.stringify({
            engineName, name, url: page.url(), title: await page.title(),
            body: (await page.locator('body').innerText()).slice(0, 1500),
            player: await page.locator('.video-js').evaluate(wrapper => ({
              classes: wrapper.className,
              html: wrapper.outerHTML.slice(0, 1600),
              video: [...wrapper.querySelectorAll('video')].map(video => ({
                paused: video.paused, currentTime: video.currentTime,
                readyState: video.readyState, width: video.videoWidth,
                error: video.error?.message ?? null,
              })),
            })).catch(() => null),
            videoCount: await page.locator('video').count(), errors, badResponses, mediaRequests,
          }));
          throw error;
        }
        const startupMs = Math.round(performance.now() - navigationStart);
        const initial = await player.evaluate(video => ({
          width: video.videoWidth, height: video.videoHeight,
          readyState: video.readyState, duration: video.duration,
          currentTime: video.currentTime,
        }));
        assert(initial.duration >= (audioOnly ? 8 : 4),
          `unexpected duration: ${JSON.stringify(initial)}`);
        assert.equal(initial.width, 640);
        assert.equal(initial.height, audioOnly ? 360 : 480);
        await player.evaluate(video => video.pause());

        if (!audioOnly) {
          // PeerTube's actual quality menu must expose the two remote fMP4 files.
          await page.locator('.vjs-tech').hover({ force: true });
          await page.locator('.vjs-settings').click();
          await page.locator('.vjs-main-menu .vjs-menu-item')
            .filter({ hasText: 'Quality' }).click();
          const qualityMenu = page.locator('.vjs-menu').filter({
            has: page.locator('.vjs-back-button').filter({ hasText: 'Quality' }),
          });
          await qualityMenu.locator('[role="menuitemradio"]', { hasText: '360p' })
            .waitFor({ state: 'visible' });
          await qualityMenu.locator('[role="menuitemradio"]', { hasText: '480p' })
            .waitFor({ state: 'visible' });
          await qualityMenu.locator('[role="menuitemradio"]', { hasText: '240p' })
            .waitFor({ state: 'visible' });
          await qualityMenu.locator('[role="menuitemradio"]', { hasText: '360p' }).click();
          await page.waitForFunction(() => {
            const quality = [...document.querySelectorAll('.vjs-menu')]
              .find(menu => menu.querySelector('.vjs-back-button')?.textContent.trim() === 'Quality');
            return quality?.querySelector('[role="menuitemradio"][aria-checked="true"]')
              ?.textContent.trim().startsWith('360p');
          }, undefined, { timeout: 10_000 });
        }

        // Use the player's seek control, then confirm it decodes again.
        await page.locator('.vjs-tech').hover({ force: true });
        const progress = page.locator('.vjs-progress-holder');
        const box = await progress.boundingBox();
        assert(box && box.width > 20, 'PeerTube seek bar unavailable');
        if (!audioOnly) {
          await progress.hover({position:{x:box.width * 0.55,y:box.height / 2},force:true});
          await page.waitForFunction(() => {
            const sprite = document.querySelector('.vjs-storyboard-sprite-placeholder');
            return sprite && getComputedStyle(sprite).backgroundImage.includes('url(')
              && sprite.getBoundingClientRect().height > 20;
          }, undefined, {timeout:15_000});
        }
        await progress.click({ position: { x: box.width * 0.55, y: box.height / 2 } });
        await page.waitForFunction(() => document.querySelector('video.vjs-tech')?.currentTime > 1.5,
          undefined, { timeout: 15_000 });
        await player.evaluate(video => video.play());
        await page.waitForFunction(() => {
          const video = document.querySelector('video.vjs-tech');
          return video && video.readyState >= 2 && video.currentTime > 1.7 && !video.error;
        }, undefined, { timeout: 20_000 });
        const afterSeek = await player.evaluate(video => ({
          width: video.videoWidth, height: video.videoHeight,
          currentTime: video.currentTime, readyState: video.readyState,
        }));
        await player.evaluate(video => video.pause());
        if (engineName === 'chromium' && name === 'desktop') {
          const evidenceDirectory = fileURLToPath(new URL('../target/e2e/', import.meta.url));
          mkdirSync(evidenceDirectory, { recursive: true });
          await player.screenshot({ path: join(evidenceDirectory,
            audioOnly ? 'peertube-native-audio-frame.png' : 'peertube-native-hls-frame.png') });
        }
        assert.equal(afterSeek.width, audioOnly ? 640 : 480);
        assert.equal(afterSeek.height, 360);
        assert.equal(errors.length, 0, `PeerTube page errors: ${errors.join('; ')}`);
        assert.deepEqual(badResponses.filter(response => response.origin === 'https://plamenu.local'), [],
          'remote Plamenu media or metadata request failed');
        // HLS.js may cancel an outstanding range request when quality changes
        // or the short fixture ends; that is not a failed media response.
        assert.deepEqual(mediaRequests.filter(request =>
          request.failure && !/ERR_ABORTED|NS_BINDING_ABORTED/.test(request.failure)), [],
        'remote Plamenu request failed');
        assert(mediaRequests.some(request => request.url.includes('/master.m3u8') && request.status === 200));
        if (!audioOnly) {
          assert(mediaRequests.some(request => request.url.includes('/native/480/index.m3u8') && request.status === 200));
        }
        assert(mediaRequests.some(request => request.url.includes('/native/360/index.m3u8') && request.status === 200));
        assert(mediaRequests.some(request => request.url.includes('.m4s') && request.status === 206));
        results.push({ engineName, name, startupMs, initial, afterSeek, badResponses });
      } finally {
        await context.close();
      }
    }
  } finally {
    await browser.close();
  }
}
console.log(JSON.stringify(results));
