import { execFileSync } from 'node:child_process';
import { test, expect } from '../fixtures.mjs';
import { uiFixture } from '../ui-fixture.mjs';

const labels = Object.fromEntries(['play', 'pause', 'seek', 'mute', 'unmute', 'volume', 'captions', 'off', 'fullscreen', 'settings', 'speed', 'pip', 'live'].map(key => [key, key[0].toUpperCase() + key.slice(1)]));
const player = JSON.stringify({ duration: null, labels }).replaceAll('"', '&quot;');
const image = '<svg xmlns="http://www.w3.org/2000/svg" width="240" height="140"><rect width="240" height="140" fill="blue"/></svg>';
const clip = execFileSync('ffmpeg', ['-hide_banner', '-loglevel', 'error', '-f', 'lavfi', '-i', 'color=c=blue:s=160x90:r=10', '-t', '2', '-c:v', 'libx264', '-pix_fmt', 'yuv420p', '-movflags', 'frag_keyframe+empty_moov', '-f', 'mp4', 'pipe:1']);

for (const prefix of ['', '@no-js ']) {
  test(`${prefix}inline images and video share attachment viewers and native fallbacks`, async ({ page }) => {
    const errors = await uiFixture(page, `<main style="max-width:600px;padding:12px;margin:auto"><article class="status"><div class="status__content">
      <p>Before the illustration</p><span class="status__inline-media"><a class="media__link" href="/image.svg" target="_blank" rel="noopener"><img class="status__inline-image" src="/image.svg" alt="Inline blue illustration" data-media-id="1"></a></span>
      <p>Before the player</p><div class="status__media" data-count="1"><figure class="media media--video"><video src="/movie.mp4" controls playsinline preload="none"></video><span hidden data-vod-controls="${player}"></span></figure></div><p>After the player</p>
      </div></article></main>`, { '/image.svg': { contentType: 'image/svg+xml', body: image }, '/movie.mp4': { contentType: 'video/mp4', body: clip } });
    const tile = page.locator('.status__inline-media a');
    if (prefix) {
      const opened = page.waitForEvent('popup');
      await tile.click();
      const popup = await opened;
      await expect(popup).toHaveURL(/\/image\.svg$/);
      await popup.close();
      await expect(page.locator('video')).toHaveAttribute('controls');
      await page.locator('video').press('Space');
    } else {
      await tile.click();
      await expect(page.getByRole('dialog', { name: 'Media viewer' })).toBeVisible();
      await page.keyboard.press('Escape');
      await expect(page.locator('.vod-controls')).toHaveCount(1);
      await page.locator('.vod-controls__play').click();
    }
    await expect.poll(() => page.locator('video').first().evaluate(video => video.currentTime)).toBeGreaterThan(.1);
    expect(await page.locator('video').first().evaluate(video => video.error?.code ?? null)).toBeNull();
    await expect(page.getByText('After the player', { exact: true })).toBeVisible();
    expect(errors).toEqual([]);
  });
}
