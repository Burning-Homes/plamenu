import {test, expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {signIn, freshMusicAudio, playNativeAudio} from '../session.mjs';
import {randomUUID} from 'node:crypto';

async function audit(page, noScript) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  if (!noScript) {
    const result = await new AxeBuilder({page}).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
    expect(result.violations.map(v => ({id: v.id, targets: v.nodes.map(n => n.target)}))).toEqual([]);
  }
}

async function journey(page, info, noScript) {
  test.setTimeout(180000);
  await signIn(page);
  await page.goto('/music');
  await expect(page.getByRole('searchbox', {name: 'Search music or paste a link', exact: true})).toBeVisible();
  await expect(page.getByRole('link', {name: 'Live radio', exact: true})).toBeVisible();
  await expect(page.getByRole('link', {name: 'Podcasts', exact: true})).toBeVisible();
  await page.getByRole('link', {name: 'Your library', exact: true}).click();
  await page.getByRole('link', {name: 'Upload audio', exact: true}).press('Enter');
  const upload = page.locator('form[action="/web/music/assets/import"]');
  await expect(upload).toBeVisible();
  const title = 'Listener journey ' + randomUUID();
  const audioFile = await freshMusicAudio();
  await upload.getByLabel('Audio file', {exact: true}).setInputFiles(audioFile);
  await upload.getByLabel('Title (optional; use audio tags or file name)', {exact: true}).fill(title);
  await upload.getByText('More music details (optional)',{exact:true}).click();
  await upload.getByLabel('Artist', {exact: true}).fill('Listener artist');
  await upload.getByLabel('Album', {exact: true}).fill('Listener album');
  await upload.getByRole('button', {name: 'Publish now', exact: true}).press('Enter');
  await page.waitForURL(url => /^\/music\/assets\/\d+$/.test(url.pathname));
  const assetUrl = page.url();
  await expect(page.getByRole('heading', {name: title, exact: true})).toBeVisible();
  await expect(page.getByText('Published. People can find and listen to this audio; it is also in your library.', {exact: true})).toBeVisible();
  await page.screenshot({path:info.outputPath('music-track-details.png'),fullPage:true,animations:'disabled',timeout:30000});
  await page.getByRole('link', {name: 'Listener album', exact: true}).press('Enter');
  await page.screenshot({path:info.outputPath('music-album.png'),fullPage:true,animations:'disabled',timeout:30000});
  const windowCount = page.context().pages().length;
  await page.getByRole('button', {name: 'Play these tracks', exact: true}).press('Enter');
  if (noScript) await expect(page).toHaveURL(/\/music\/player\?autoplay=true$/);
  else await expect(page.locator('.music-dock')).toBeVisible();
  await expect(page.locator('.music-player-now')).toContainText(title);
  expect(page.context().pages()).toHaveLength(windowCount);
  await page.goto('/music/assets?mine=true&upload=true');
  await upload.getByLabel('Audio file', {exact: true}).setInputFiles(audioFile);
  await upload.getByLabel('Title (optional; use audio tags or file name)', {exact: true}).fill('Do not rename the existing public recording');
  await upload.getByRole('button', {name: 'Save privately', exact: true}).press('Enter');
  await expect(page).toHaveURL(assetUrl+'?reused=true');
  await expect(page.getByRole('heading', {name: title, exact: true})).toBeVisible();
  await expect(page.getByText('You already uploaded this audio. This is your existing track; its title and access have been kept. Use Edit track to change it.', {exact: true})).toBeVisible();
  await page.goto('/music');
  await page.getByRole('searchbox', {name: 'Search music or paste a link', exact: true}).fill(title);
  await page.getByRole('button', {name: 'Search', exact: true}).press('Enter');
  const result = page.locator('.music-listening-results > li').filter({has: page.getByRole('heading', {name: title, exact: true})});
  await expect(result).toHaveCount(1);
  await expect(result.getByRole('button', {name: 'Play', exact: true})).toBeVisible();
  await audit(page, noScript);
  await page.screenshot({path: info.outputPath('music-listening-hub.png'), fullPage: true,animations:'disabled',timeout:30000});
  await page.getByRole('link', {name: 'Your library', exact: true}).first().press('Enter');
  const saved = page.locator('.music-personal-items > li').filter({has: page.getByRole('link', {name: title, exact: true})});
  await expect(saved).toHaveCount(1);
  await expect(saved.locator(".music-membership__kind")).toHaveText("Song");
  await expect(page.locator(".music-personal-items > li").first().getByRole("button",{name:"Play",exact:true})).toBeVisible();
  await saved.getByRole('button', {name: 'Play', exact: true}).press('Enter');
  if (noScript) await expect(page).toHaveURL(/\/music\/player\?autoplay=true$/);
  else await expect(page.locator('.music-dock')).toBeVisible();
  expect(page.context().pages()).toHaveLength(windowCount);
  const player = page;
  try {
    await expect(player.locator('.music-player-now')).toContainText(title);
    await expect(player.locator('.music-player-entries > li')).toHaveCount(1);
    await expect(player.locator('#music-player-destination')).toBeHidden();
    await expect(player.getByRole('button', {name: 'Save player settings', exact: true})).toBeHidden();
    await expect(player.getByRole('button', {name: 'Move up', exact: true})).toBeHidden();
    const audio = player.locator('audio.music-player-audio');
    await playNativeAudio(audio, info);
    await expect.poll(() => audio.evaluate(a => a.currentTime), {timeout: 15000}).toBeGreaterThan(0);
    expect(await audio.evaluate(a => a.error?.code ?? null)).toBeNull();
    if (!noScript) {
      const before = await audio.evaluate(a => a.currentTime);
      await player.getByRole('link', {name: 'Discover', exact: true}).first().click();
      await expect(page.getByRole('heading', {name: 'Music', exact: true})).toBeVisible();
      await expect.poll(() => audio.evaluate(a => a.currentTime)).toBeGreaterThan(before);
      expect(page.context().pages()).toHaveLength(windowCount);
      await page.locator('.music-dock__actions').getByRole('button', {name: 'Queue', exact: true}).click();
    }
    await audit(player, noScript);
    await player.screenshot({path: info.outputPath('music-immediate-player.png'), fullPage: true,animations:'disabled',timeout:30000});
    await player.getByText('Advanced playback', {exact: true}).press('Enter');
    await expect(player.getByLabel('Position in seconds', {exact: true})).toBeVisible();
    await expect(player.getByLabel('Playback destination', {exact: true})).toBeVisible();
  } finally {
    await page.goto('/music');
  }
}

test('ordinary listeners can publish, find and play audio without queue setup', async ({page}, info) => journey(page, info, false));
test('ordinary audio journey works without JavaScript @no-js', async ({page}, info) => journey(page, info, true));
