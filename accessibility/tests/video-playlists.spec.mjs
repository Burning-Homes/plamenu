import { test, expect } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import { randomUUID } from 'node:crypto';
import { signIn, openVideoOptions, createVideoChannel ,videoChannelForm} from '../session.mjs';

async function scan(page, noScript) {
  const widths = await page.evaluate(() => [innerWidth, document.documentElement.scrollWidth]);
  expect(widths[1]).toBeLessThanOrEqual(widths[0]);
  if (!noScript) {
    const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
    expect(result.violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) }))).toEqual([]);
  }
}
function form(page, op) { return page.locator(`form:has(input[name="operation"][value="${op}"])`); }

async function journey(page, info) {
  test.setTimeout(180_000);
  const noScript = info.project.use.javaScriptEnabled === false;
  await signIn(page);
  await page.goto('/videos/upload');await openVideoOptions(page);
  const project = info.project.name.split('-').map(word=>word[0]).join('');
  const channelName = `clips_${Date.now().toString(36)}_${project}_${randomUUID().slice(0,6)}`;
  const channel = await videoChannelForm(page);
  if (!await channel.isVisible()) await channel.locator('..').locator(':scope > summary').click();
  await channel.getByLabel('Channel address').fill(channelName);
  await channel.getByLabel('Channel name').fill(`Clip films ${channelName}`);
  await createVideoChannel(page, channel.getByRole('button', { name: 'Create a video channel' }));
  await page.waitForURL(url => url.pathname === '/videos/upload');await openVideoOptions(page);
  let upload = page.locator('form[action="/web/videos"]');
  await upload.getByRole('combobox', { name: 'Channel', exact: true }).selectOption({ label: `Clip films ${channelName}` });
  const title = `A clip film ${channelName}`;
  await upload.getByLabel('Video title').fill(title);
  await upload.getByRole('textbox', { name: 'Description', exact: true }).fill('A brief moving test pattern and audible tone.');
  await upload.getByLabel('Visual description').fill('A changing colour test pattern.');
  await upload.getByLabel('Transcript', { exact: true }).fill('A steady tone plays.');
  await upload.getByLabel('Video or audio file', { exact: true }).setInputFiles(process.env.A11Y_VIDEO_FIXTURE);
  await upload.getByLabel('Captions (WebVTT)').setInputFiles({ name: 'captions.vtt', mimeType: 'text/vtt', buffer: Buffer.from('WEBVTT\n\n00:00:00.000 --> 00:00:03.000\n[Steady tone]\n') });
  await upload.getByRole('button', { name: 'Upload video', exact: true }).press('Enter');
  await page.waitForURL(url => /\/videos\/drafts\/\d+$/.test(url.pathname));
  if (noScript) {
    for (let attempt = 0; attempt < 60; attempt++) {
      if (await page.getByRole('link', { name: 'Review your video', exact: true }).isVisible()) break;
      await page.getByRole('link', { name: 'Refresh status', exact: true }).click();
      await page.waitForTimeout(500);
    }
  } else await expect(page.getByRole('link', { name: 'Review your video', exact: true })).toBeVisible({ timeout: 45_000 });
  await page.getByRole('link', { name: 'Review your video', exact: true }).click();
  await page.locator('form[action="/web/videos"]').getByRole('button', { name: 'Publish video', exact: true }).press('Enter');
  await page.waitForURL(url => /\/@[^/]+\/\d+/.test(url.pathname));
  const videoUrl = page.url();
  await page.goto('/videos/explore');
  await page.getByRole('link', { name: 'Video playlists', exact: true }).first().click();
  const create = page.locator('form[action="/web/video-playlists"]');
  if (await page.locator('.video-workspace__create').getAttribute('open') === null) await page.locator('.video-workspace__create > summary').press('Enter');
  await create.getByRole('combobox', { name: 'Video channel' }).selectOption({ label: `Clip films ${channelName}` });
  await create.getByLabel('Playlist title').fill(`Clip selection ${channelName}`);
  await create.getByRole('textbox', { name: 'Playlist description' }).fill('Two accessible occurrences of one film.');
  await create.getByRole('combobox', { name: 'Visibility' }).selectOption('public');
  await create.getByRole('button', { name: 'Create playlist', exact: true }).press('Enter');
  await page.waitForURL(url => /\/videos\/playlists\/\d+$/.test(url.pathname));
  const playlistUrl = page.url();
  for (const [start,stop] of [['0','1'],['1','2']]) {
    const add = form(page,'add');
    await add.getByLabel('Video URL', { exact: true }).fill(videoUrl);
    await add.getByLabel('Clip start (whole seconds)').fill(start);
    await add.getByLabel('Clip end (whole seconds, optional)').fill(stop);
    await add.getByRole('button', { name: 'Add Video', exact: true }).press('Enter');
    await expect(page.locator('.video-playlist__entries > li')).toHaveCount(Number(start) + 1);
  }
  const identities = await page.locator('.video-playlist__entries > li').evaluateAll(entries => entries.map(e => e.id));
  await page.setViewportSize({ width: 320, height: 900 });
  expect(new Set(identities).size).toBe(2);
  const second = page.locator(`#${identities[1]}`);
  await second.locator('summary').filter({ hasText: /^Edit entry 2$/ }).press('Enter');
  const move = second.locator('form[data-playlist-move]');
  if (noScript) {
    await move.getByLabel('New position').fill('1');
    await move.getByRole('button', { name: 'Move entry', exact: true }).press('Enter');
  } else await move.getByRole('button', { name: 'Move entry up', exact: true }).press('Enter');
  await expect(page.locator('.video-playlist__entries > li').first()).toHaveAttribute('id', identities[1]);
  await expect(page.locator('.video-playlist__entries > li').last()).toHaveAttribute('id', identities[0]);
  await scan(page,noScript);
  const player = page.locator('.video-playlist__player video').first();
  expect(await (await page.request.get(page.url())).text()).toContain('#t=1,2');
  if (noScript) expect(await player.getAttribute('src')).toContain('#t=1,2');
  // The normal JS player intentionally pauses the initial MP4 play request
  // while handing over to HLS. Verify decoded playback, not that promise.
  await player.evaluate(async video => {
    video.muted = true;
    try { await video.play(); } catch (error) { if (error.name !== 'AbortError') throw error; }
  });
  await expect.poll(() => player.evaluate(video => video.getVideoPlaybackQuality().totalVideoFrames), { timeout: 15_000 }).toBeGreaterThan(0);
  await expect.poll(() => player.evaluate(video => video.paused && video.currentTime >= 1.9), { timeout: 15_000 }).toBe(true);
  const ending = await player.evaluate(video => ({ time: video.currentTime, frames: video.getVideoPlaybackQuality().totalVideoFrames }));
  expect(ending.time).toBeGreaterThanOrEqual(1.9);
  expect(ending.time).toBeLessThan(2.5);
  expect(ending.frames).toBeGreaterThan(0);
  if (!noScript) await expect(page.locator('[data-playlist-status]')).toContainText('Clip finished');
  await page.getByRole('link', { name: 'Next entry', exact: true }).press('Enter');
  await expect(page.getByRole('heading', { name: 'Now playing entry 2' })).toBeVisible();
  expect(await (await page.request.get(page.url())).text()).toContain('#t=0,1');
  if (noScript) await expect(page.locator('.video-playlist__player video').first()).toHaveAttribute('src', /#t=0,1$/);
  await page.screenshot({ path: info.outputPath('playlist-320.png'), fullPage: true });
  await page.locator('summary').filter({ hasText: /^Playlist settings$/ }).press('Enter');
  const profile = form(page,'profile');
  await profile.getByRole('combobox', { name: 'Visibility' }).selectOption('private');
  await profile.getByRole('button', { name: 'Save playlist', exact: true }).press('Enter');
  const anonymous = await page.context().browser().newContext({ baseURL: new URL(playlistUrl).origin, ignoreHTTPSErrors: false, javaScriptEnabled: !noScript });
  expect((await anonymous.request.get(playlistUrl)).status()).toBe(404);
  await anonymous.close();
  await page.locator('summary').filter({ hasText: /^Playlist settings$/ }).press('Enter');
  const deletion = form(page,'delete');
  await deletion.getByLabel('Type the playlist title to confirm').fill(`Clip selection ${channelName}`);
  await deletion.getByRole('button', { name: 'Delete playlist', exact: true }).press('Enter');
  await page.waitForURL(url => url.pathname === '/videos/playlists' && url.search.includes('mine=true'));
  expect((await page.request.get(playlistUrl)).status()).toBe(404);
  expect((await page.request.get(videoUrl)).status()).toBe(200);
}
test('keyboard playlist authoring, stable duplicate clips, native playback and confirmed deletion', async ({page},info)=>journey(page,info));
test('playlist authoring and native clipped playback without scripts @no-js', async ({page},info)=>journey(page,info));
