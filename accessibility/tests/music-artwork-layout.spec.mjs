import {test, expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {deflateSync} from 'node:zlib';
import {randomUUID} from 'node:crypto';
import {signIn, freshMusicAudio, playNativeAudio, openDetailsFor} from '../session.mjs';

// Real intrinsic-size artwork, uploaded through the product. A 96px fixture
// cannot expose the full-size image squeezing the adjacent title into letters.
function largeArtwork() {
  function chunk(type, bytes) {
    const data = Buffer.concat([Buffer.from(type), bytes]);
    let crc = 0xffffffff;
    for (const byte of data) {
      crc ^= byte;
      for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ ((crc & 1) ? 0xedb88320 : 0);
    }
    const result = Buffer.alloc(data.length + 8);
    result.writeUInt32BE(bytes.length);
    data.copy(result, 4);
    result.writeUInt32BE((crc ^ 0xffffffff) >>> 0, result.length - 4);
    return result;
  }
  const header = Buffer.alloc(13);
  header.writeUInt32BE(1024, 0); header.writeUInt32BE(1024, 4);
  header[8] = 8; header[9] = 2;
  const pixels = Buffer.alloc((1024 * 3 + 1) * 1024);
  for (let y = 0; y < 1024; y++) {
    for (let x = 0; x < 1024; x++) {
      const offset = y * (1024 * 3 + 1) + 1 + x * 3;
      pixels[offset] = x % 256; pixels[offset + 1] = y % 256; pixels[offset + 2] = 96;
    }
  }
  return {name: 'large-artwork.png', mimeType: 'image/png', buffer: Buffer.concat([
    Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
    chunk('IHDR', header), chunk('IDAT', deflateSync(pixels)), chunk('IEND', Buffer.alloc(0)),
  ])};
}

async function journey(page, info, noScript) {
  test.setTimeout(180000);
  await signIn(page);
  await page.goto('/music/assets?mine=true&upload=true');
  const upload = page.locator('form[action="/web/music/assets/import"]');
  const title = 'Killing In the Name — Rage Against The Machine ' + randomUUID().slice(0, 8);
  await upload.getByLabel('Audio file', {exact: true}).setInputFiles(await freshMusicAudio());
  await upload.getByLabel('Title (optional; use audio tags or file name)', {exact: true}).fill(title);
  await upload.getByText('More music details (optional)',{exact:true}).click();
  await upload.getByLabel('Artist', {exact: true}).fill('Rage Against The Machine');
  await upload.getByRole('button', {name: 'Publish now', exact: true}).press('Enter');
  await expect(page.locator('section.music-library > audio')).toHaveCount(1);
  await expect(page.locator('form[action="/web/music/play"]')).toHaveCount(0);
  await openDetailsFor(page.getByText('Change artwork', {exact: true}));
  await page.getByLabel('Image file', {exact: true}).setInputFiles(largeArtwork());
  await page.getByRole('button', {name: 'Upload image', exact: true}).press('Enter');
  await page.goto('/music?q=' + encodeURIComponent(title));
  const row = page.locator('.music-listening-results > li').filter({has: page.getByRole('heading', {name: title, exact: true})});
  await expect(row).toHaveCount(1);
  await expect.poll(() => row.locator('img').evaluate(img => img.complete && img.naturalWidth)).toBe(1024);
  for (const width of [320, 390, 560, 1440]) {
    await page.setViewportSize({width, height: 800});
    const geometry = await row.evaluate(li => {
      const artwork = li.querySelector('img').getBoundingClientRect();
      const text = li.querySelector('div').getBoundingClientRect();
      const button = li.querySelector('button').getBoundingClientRect();
      return {artwork: artwork.width, text: text.width, playWidth: button.width,
        playHeight: button.height, rowHeight: li.getBoundingClientRect().height,
        scroll: document.documentElement.scrollWidth, viewport: innerWidth};
    });
    expect(geometry.artwork, JSON.stringify(geometry)).toBeLessThanOrEqual(64);
    expect(geometry.text, JSON.stringify(geometry)).toBeGreaterThanOrEqual(140);
    expect(geometry.playWidth).toBeGreaterThanOrEqual(44);
    expect(geometry.playHeight).toBeLessThanOrEqual(60);
    expect(geometry.rowHeight).toBeLessThanOrEqual(230);
    expect(geometry.scroll).toBeLessThanOrEqual(geometry.viewport);
    expect(geometry.viewport).toBe(width);
    if (!noScript) {
      const audit = await new AxeBuilder({page}).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
      expect(audit.violations.map(v => ({id: v.id, targets: v.nodes.map(n => n.target)}))).toEqual([]);
    }
    await page.screenshot({path: info.outputPath(`artwork-${width}.png`), fullPage: true});
  }
  const post = page.locator('.status').filter({hasText: title});
  await expect(post.locator('audio')).toHaveCount(1);
  await expect(post.getByRole('button', {name: 'Play', exact: true})).toHaveCount(0);
  await expect(post.getByRole('button', {name: 'Like', exact: true})).toBeHidden();
  await expect(post.getByRole('link', {name: 'Open in music player', exact: true})).toBeHidden();
  await page.setViewportSize({width: 390, height: 800});
  await page.emulateMedia({colorScheme: 'dark'});
  if (!noScript) {
    await page.evaluate(async()=>{
      const transitions=[...document.querySelectorAll('.music-search button, .music-play-now button')]
        .flatMap(button=>button.getAnimations()).filter(animation=>animation instanceof CSSTransition && animation.playState==='running');
      await Promise.race([Promise.all(transitions.map(animation=>animation.finished.catch(()=>{}))),new Promise(resolve=>setTimeout(resolve,500))]);
    });
    const darkAudit = await new AxeBuilder({page}).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
    expect(darkAudit.violations.map(v => ({id: v.id, targets: v.nodes.map(n => n.target)}))).toEqual([]);
  }
  await page.screenshot({path: info.outputPath('artwork-390-dark.png'), fullPage: true});
  await post.scrollIntoViewIfNeeded();
  await page.screenshot({path: info.outputPath('audio-post-primary-player.png'), fullPage: true});
  await playNativeAudio(post.locator('audio'), info);
  await expect.poll(() => post.locator('audio').evaluate(a => a.currentTime), {timeout: 15000}).toBeGreaterThan(0);
  expect(await post.locator('audio').evaluate(a => a.error?.code ?? null)).toBeNull();
  await post.getByText('Music options', {exact: true}).press('Enter');
  await expect(post.getByRole('button', {name: 'Like', exact: true})).toBeVisible();
  await expect(post.getByRole('link', {name: 'Open in music player', exact: true})).toBeVisible();
}

test('large artwork leaves readable titles and a single primary post player', async ({page}, info) => journey(page, info, false));
test('large artwork and audio post work without JavaScript @no-js', async ({page}, info) => journey(page, info, true));
