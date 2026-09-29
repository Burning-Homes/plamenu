import { expect, test } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import { execFileSync } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { createServer } from 'node:http';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { signIn } from '../session.mjs';

let fixtureDirectory;
let fixture;
let audioFixture;
let posterFixture;
test.beforeAll(() => {
  fixtureDirectory = mkdtempSync(join(tmpdir(), 'plamenu-native-video-'));
  if (process.env.A11Y_VIDEO_FIXTURE) {
    fixture = process.env.A11Y_VIDEO_FIXTURE;
  } else {
    fixture = join(fixtureDirectory, 'sample.mp4');
    execFileSync('ffmpeg', ['-v', 'error', '-f', 'lavfi', '-i', 'testsrc2=size=640x480:rate=10',
      '-f', 'lavfi', '-i', 'sine=frequency=440', '-t', '3', '-c:v', 'libx264', '-pix_fmt', 'yuv420p',
      '-c:a', 'aac', '-movflags', '+faststart', fixture]);
  }
  if (process.env.A11Y_AUDIO_FIXTURE) {
    audioFixture = process.env.A11Y_AUDIO_FIXTURE;
  } else {
    audioFixture = join(fixtureDirectory, 'tone.m4a');
    execFileSync('ffmpeg', ['-v', 'error', '-f', 'lavfi', '-i', 'sine=frequency=440:duration=9',
      '-c:a', 'aac', audioFixture]);
  }
  if (process.env.A11Y_POSTER_FIXTURE) {
    posterFixture = process.env.A11Y_POSTER_FIXTURE;
  } else {
    posterFixture = join(fixtureDirectory, 'poster.png');
    execFileSync('ffmpeg', ['-v', 'error', '-f', 'lavfi', '-i', 'color=c=blue:size=96x96',
      '-frames:v', '1', posterFixture]);
  }
});
test.afterAll(() => {
  if (fixtureDirectory) rmSync(fixtureDirectory, { recursive: true });
});

async function publish(page, projectName, noScript) {
  await signIn(page);
  await page.goto('/videos');
  await expect(page.getByRole('heading', { name: 'Videos', exact: true })).toBeVisible();
  const name = `film_${randomUUID().replaceAll('-', '').slice(0, 20)}`;
  let channelName = `Films ${name}`;
  let form = page.locator('form[action="/web/videos"]');
  const ownedCount = await form.getByRole('combobox', { name: 'Channel', exact: true }).locator('option').count();
  if (ownedCount >= 50) {
    // Long-running local matrices retain evidence channels. Reuse one at the
    // supported cap instead of deleting existing local-instance data.
    channelName = (await form.getByRole('combobox', { name: 'Channel', exact: true })
      .locator('option').first().textContent()).trim();
  } else {
    const channelForm = page.locator('form[action="/web/video-channels"]');
    if (!await channelForm.isVisible()) await page.locator('summary').filter({ hasText: 'Create a video channel' }).click();
    await channelForm.getByLabel('Channel address').fill(name);
    await channelForm.getByLabel('Channel name').fill(`Films ${name}`);
    await channelForm.getByRole('button', { name: 'Create a video channel' }).click();
    await page.waitForURL(url => url.pathname === '/videos');
    form = page.locator('form[action="/web/videos"]');
  }
  await form.getByRole('combobox', { name: 'Channel', exact: true }).selectOption({ label: channelName });
  await expect(page.getByRole('navigation', { name: 'Your video channels' })
    .getByRole('link', { name: channelName, exact: true }))
    .toHaveAttribute('href', /\/videos\/explore\?channel=\d+/);
  await form.getByLabel('Video title').fill(`Browser video ${name}`);
  await form.getByLabel('Description', { exact: true }).fill('Native video from an ordinary HTML form.');
  await form.getByLabel('Language code').fill('en');
  await form.getByLabel('Video or audio file', { exact: true }).setInputFiles(fixture);
  const posterTime = form.getByLabel('Poster frame time (seconds)');
  if (noScript) {
    await posterTime.fill('1.5');
  } else {
    const picker = form.locator('[data-poster-picker]');
    await expect(picker).toBeVisible();
    const source = picker.locator('video');
    await expect.poll(() => source.evaluate(video => video.duration)).toBeGreaterThan(2);
    await source.evaluate(video => { video.pause(); video.currentTime = 1.5; });
    await expect.poll(() => source.evaluate(video => video.currentTime)).toBeGreaterThan(1.3);
    await picker.getByRole('button', { name: 'Use this frame' }).click();
    await expect(posterTime).toHaveValue('1.5');
  }
  await form.getByLabel('Visual description').fill('A moving colour test pattern.');
  await form.getByLabel('Transcript', { exact: true }).fill('A steady test tone plays throughout.');
  await form.getByLabel('Captions (WebVTT)').setInputFiles({ name: 'captions.vtt', mimeType: 'text/vtt',
    buffer: Buffer.from('WEBVTT\n\n00:00:00.000 --> 00:00:03.000\n[Steady test tone]\n') });
  if (!noScript) {
    await page.route('**/web/videos/resumable', async route => {
      await new Promise(resolve => setTimeout(resolve, 700));
      await route.continue();
    });
  }
  await form.getByRole('button', { name: 'Upload video' }).click();
  if (!noScript) {
    await expect(form.locator('[data-video-upload-progress]')).toBeVisible();
    await expect(form.locator('[data-video-upload-label]')).toContainText(/Uploading|Upload received/);
    const duringUpload = await page.evaluate(() => ({ width: innerWidth, content: document.documentElement.scrollWidth }));
    expect(duringUpload.content).toBeLessThanOrEqual(duringUpload.width);
  }
  await page.waitForURL(url => /\/videos\/drafts\/\d+$/.test(url.pathname));
  if (!noScript) await page.unroute('**/web/videos/resumable');
  const draftId = new URL(page.url()).pathname.split('/').pop();
  expect((await page.request.get(`/media/${draftId}.orig`)).status()).toBe(404);
  await expect(page.getByText('Only you can access this draft. It has not been published.')).toBeVisible();
  if (noScript) {
    for (let attempt = 0; attempt < 60; attempt++) {
      if (await page.getByRole('link', { name: 'Review your video', exact: true }).isVisible()) break;
      await page.getByRole('link', { name: 'Refresh status', exact: true }).click();
      await page.waitForTimeout(500);
    }
  } else {
    await expect(page.getByRole('link', { name: 'Review your video', exact: true }))
      .toBeVisible({ timeout: 45_000 });
  }
  await page.getByRole('link', { name: 'Review your video', exact: true }).click();
  const review = page.locator('form[action="/web/videos"]');
  await expect(page.getByRole('heading', { name: 'Review your video' })).toBeVisible();
  await expect(page.locator('video source')).toHaveAttribute('src', /\/videos\/drafts\/\d+\/preview\/video$/);
  await expect(page.getByRole('img', { name: 'Selected poster frame' })).toBeVisible();
  await expect(page.locator('.video-publish__poster')).toContainText('1.5 s');
  const replacement = page.locator('form[data-video-poster-replace]');
  await expect(replacement).toBeVisible();
  await replacement.getByLabel('Describe the poster image, including any text shown in it')
    .fill('A solid blue square.');
  await replacement.getByLabel('Still image for the poster').setInputFiles(posterFixture);
  if (!noScript) await expect(replacement.getByRole('img', { name: 'Preview of the new poster image' })).toBeVisible();
  await replacement.getByRole('button', { name: 'Save custom poster' }).click();
  await expect(page.getByRole('img', { name: 'A solid blue square.' })).toBeVisible();
  await expect(page.locator('.video-publish__poster')).toContainText('Custom poster');
  expect((await page.request.get(`/media/${draftId}.orig`)).status()).toBe(404);
  await expect(page.locator('video track')).toHaveAttribute('kind', 'captions');
  await expect(review.getByLabel('Video title')).toHaveValue(`Browser video ${name}`);
  const reflow = await page.evaluate(() => ({ width: innerWidth, content: document.documentElement.scrollWidth }));
  expect(reflow.content).toBeLessThanOrEqual(reflow.width);
  if (!noScript) {
    const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
    expect(result.violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) }))).toEqual([]);
  }
  await review.getByRole('button', { name: 'Publish video', exact: true }).click();
  await page.waitForURL(url => /\/@[^/]+\/\d+/.test(url.pathname));
  expect((await page.request.get(`/media/${draftId}.orig`)).status()).toBe(404);
  await expect(page.getByText(`Browser video ${name}`, { exact: true })).toBeVisible();
  await expect(page.locator('video').first()).toBeVisible();
  await page.getByText('Poster image description', { exact: true }).click();
  await expect(page.getByText('A solid blue square.', { exact: true })).toBeVisible();
  await expect(page.locator('video track[kind="captions"]').first()).toBeAttached();
  const alt = page.getByRole('button', { name: 'ALT: A moving colour test pattern.', exact: true });
  const altBox = await alt.boundingBox();
  expect(altBox.width).toBeGreaterThanOrEqual(44);
  expect(altBox.height).toBeGreaterThanOrEqual(44);
  await alt.press('Enter');
  await expect(page.locator('.media__alt-text').filter({ hasText: 'A moving colour test pattern.' })).toBeVisible();
  const player = page.locator('video').first();
  const masterUrl = await player.getAttribute('data-hls');
  expect(masterUrl).toMatch(/\/media\/hls\/\d+\/master\.m3u8$/);
  const master = await page.request.get(masterUrl);
  expect(master.ok()).toBeTruthy();
  const masterBody = await master.text();
  expect(masterBody).toContain('RESOLUTION=640x480');
  expect(masterBody).toContain('RESOLUTION=480x360');
  const lowPlaylistPath = masterBody.split('\n').find(line => line.includes('/native/360/index.m3u8'));
  const lowPlaylist = await page.request.get(lowPlaylistPath);
  expect(lowPlaylist.ok()).toBeTruthy();
  const lowBody = await lowPlaylist.text();
  expect(lowBody).toContain('#EXT-X-BYTERANGE:');
  const manifestUrl = masterUrl.replace('/master.m3u8', '/sha256.json');
  const manifestResponse = await page.request.get(manifestUrl);
  expect(manifestResponse.ok()).toBeTruthy();
  const manifest = await manifestResponse.json();
  const [range, relativeFile] = [lowBody.match(/#EXT-X-BYTERANGE:(\d+)@(\d+)/),
    lowBody.split('\n').find(line => line.startsWith('/media/') && line.endsWith('.m4s'))];
  expect(range).toBeTruthy();
  expect(relativeFile).toBeTruthy();
  const start = Number(range[2]);
  const end = start + Number(range[1]) - 1;
  const bytes = await page.request.get(relativeFile, { headers: { Range: `bytes=${start}-${end}` } });
  expect(bytes.status()).toBe(206);
  const expectedHash = manifest[relativeFile.split('/').pop()]?.[`${start}-${end}`];
  expect(expectedHash).toBe(createHash('sha256').update(await bytes.body()).digest('hex'));
  if (!noScript) {
    expect(await player.getAttribute('data-p2p-uuid')).toMatch(/^[0-9a-f-]{36}$/);
    expect(await player.getAttribute('data-p2p-trackers')).toContain('/tracker/socket');
    await expect(page.locator('.media__p2p-toggle')).toBeVisible();
  }
  // Exercise the browser's real decoder and WebVTT engine, also with page
  // scripts disabled. This is not a claim about native-control keyboard UX.
  await player.evaluate(video => {
    video.muted = true; // Decoder check, independent of autoplay permission.
    video.textTracks[0].mode = 'showing';
    // The enhanced player intentionally cancels this first progressive play
    // while attaching HLS; the observable contract is decoded playback.
    video.play().catch(() => {});
  });
  await expect.poll(() => player.evaluate(video => video.currentTime)).toBeGreaterThan(0.1);
  await expect.poll(() => player.evaluate(video => video.textTracks[0].activeCues?.[0]?.text))
    .toBe('[Steady test tone]');
  if (!noScript) {
    const quality = page.getByRole('combobox', { name: 'Video quality' });
    if (await quality.isVisible()) {
      await expect(quality.locator('option')).toHaveText(['Auto', '240p', '360p', '480p']);
      await quality.selectOption({ label:'240p' });
      await player.evaluate(video=>{ video.currentTime=1; video.play().catch(()=>{}); });
      await expect.poll(()=>player.evaluate(video=>video.videoWidth), {timeout:15_000}).toBe(320);
      await expect.poll(()=>player.evaluate(video=>video.currentTime), {timeout:15_000}).toBeGreaterThan(1.1);
    } else {
      // Native-HLS engines retain their own controls instead of a false UI.
      expect(await player.evaluate(video=>video.canPlayType('application/vnd.apple.mpegurl'))).not.toBe('');
    }
  }
  expect(await player.evaluate(video => video.error)).toBeNull();
  await player.evaluate(video => video.pause());
  const permalink = page.url();
  const statusId = new URL(permalink).pathname.split('/').pop();
  const previews = page.locator('.media__seek-previews');
  await previews.locator('summary').press('Enter');
  await expect(previews).toContainText('Approximate keyframe previews');
  const previewLinks = previews.locator('.media__seek-grid a');
  await expect(previewLinks).toHaveCount(3);
  await expect(previewLinks.nth(1)).toHaveAttribute('href', /#t=1$/);
  await expect.poll(() => previews.locator('img').first().evaluate(image => image.naturalWidth)).toBe(480);
  // Natural dimensions alone miss the general media rule shrinking the
  // entire sprite into one tile. Its rendered dimensions must stay intact.
  await expect.poll(() => previews.locator('img').first().evaluate(image => image.getBoundingClientRect().width)).toBe(480);
  await expect.poll(() => previews.locator('img').first().evaluate(image => image.getBoundingClientRect().height)).toBe(90);
  expect(await previews.locator('.media__seek-grid').evaluate(grid => grid.scrollWidth <= grid.clientWidth + 1)).toBe(true);
  await page.setViewportSize({width:320,height:800});
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
  await previewLinks.nth(1).press('Enter');
  if (noScript) {
    await page.waitForURL(url => url.pathname.startsWith('/media/') && url.hash === '#t=1');
    const native = page.locator('video');
    await expect.poll(() => native.evaluate(video => video.currentTime)).toBeGreaterThanOrEqual(1);
    await page.goto(permalink);
  } else {
    await expect.poll(() => player.evaluate(video => video.currentTime)).toBeGreaterThanOrEqual(1);
    await expect(player).toBeFocused();
    const audit = await new AxeBuilder({page}).include('.media__seek-previews').analyze();
    expect(audit.violations).toEqual([]);
  }
  await page.setViewportSize({width:projectName.includes('mobile') ? 390 : 1440,height:900});
  if (noScript) await page.setViewportSize({ width: 320, height: 800 });
  await page.goto('/videos/explore');
  await page.getByRole('searchbox', { name: 'Search video titles and descriptions' })
    .fill(`Browser video ${name}`);
  await page.getByRole('button', { name: 'Search videos' }).click();
  await expect(page.getByRole('heading', { name: 'Explore videos' })).toBeVisible();
  const discovered = page.locator('article.video-explore__card').filter({ hasText: `Browser video ${name}` });
  await expect(discovered).toHaveCount(1);
  await expect(discovered.getByRole('link', { name: `Browser video ${name}` })).toHaveAttribute('href', new URL(permalink).pathname);
  const channelLibrary = discovered.getByRole('link', { name: channelName, exact: true });
  await expect(channelLibrary).toHaveAttribute('href', /\/videos\/explore\?channel=\d+/);
  await expect(discovered.locator('.video-explore__poster img')).toBeVisible();
  await discovered.getByText('Poster image description', { exact: true }).click();
  await expect(discovered.getByText('A solid blue square.', { exact: true })).toBeVisible();
  await expect(page.locator('.video-explore video')).toHaveCount(0);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
  if (projectName === 'chromium-mobile') {
    await discovered.scrollIntoViewIfNeeded();
    await page.screenshot({ path: '../target/e2e/video-explore-mobile.png' });
  }
  if (!noScript) {
    const audit = await new AxeBuilder({ page }).include('.video-explore').analyze();
    expect(audit.violations).toEqual([]);
  }
  await channelLibrary.click();
  await expect(page.getByRole('heading', { level: 1 })).toContainText('Videos by');
  await expect(page.getByRole('heading', { level: 1 })).toContainText(channelName);
  await expect(page.getByRole('link', { name: 'Creator profile' })).toBeVisible();
  await page.getByRole('searchbox', { name: 'Search video titles and descriptions' })
    .fill('Browser video ' + name);
  await page.getByRole('button', { name: 'Search videos' }).click();
  await expect(page).toHaveURL(/\/videos\/explore\?channel=\d+&q=/);
  await expect(page.locator('article.video-explore__card')
    .filter({ hasText: 'Browser video ' + name })).toHaveCount(1);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
  if (projectName === 'chromium-mobile') {
    await page.screenshot({ path: '../target/e2e/video-channel-library-mobile.png' });
  }
  if (!noScript) {
    const audit = await new AxeBuilder({ page }).include('.video-explore').analyze();
    expect(audit.violations).toEqual([]);
  }
  const libraryFollow = page.locator('.video-explore > form.video-explore__follow');
  if (await libraryFollow.getByRole('button', { name: 'Follow channel' }).isVisible()) {
    await libraryFollow.getByRole('button', { name: 'Follow channel' }).click();
    await expect(libraryFollow.getByRole('button', { name: 'Unfollow' })).toBeVisible();
    await libraryFollow.getByRole('button', { name: 'Unfollow' }).click();
    await expect(libraryFollow.getByRole('button', { name: 'Follow channel' })).toBeVisible();
  }
  await page.getByRole('link', { name: 'All videos' }).click();
  await page.getByRole('searchbox', { name: 'Search video titles and descriptions' })
    .fill('Browser video ' + name);
  await page.getByRole('button', { name: 'Search videos' }).click();
  await expect(discovered).toHaveCount(1);
  const follow = discovered.getByRole('button', { name: 'Follow channel' });
  const unfollow = discovered.getByRole('button', { name: 'Unfollow' });
  await expect.poll(async () => await follow.isVisible() || await unfollow.isVisible()).toBe(true);
  const wasFollowing = await unfollow.isVisible();
  if (!wasFollowing) {
    await follow.click();
    await expect(discovered.getByRole('button', { name: 'Unfollow' })).toBeVisible();
  }
  const videoViews = page.getByRole('navigation', { name: 'Video views' });
  await videoViews.locator('summary').click();
  await videoViews.getByRole('link', { name: 'Followed channels' }).click();
  await expect(page).toHaveURL(/\/videos\/explore\?q=.*&following=1/);
  await expect(page.locator('article.video-explore__card').filter({ hasText: `Browser video ${name}` })).toHaveCount(1);
  if (!wasFollowing) {
    await page.locator('article.video-explore__card').filter({ hasText: `Browser video ${name}` })
      .getByRole('button', { name: 'Unfollow' }).click();
  }
  if (noScript) await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto(permalink);
  const embedUrl = `${new URL(permalink).origin}/web/statuses/${statusId}/embed`;
  const oembedResponse = await page.request.get(`/api/oembed?url=${encodeURIComponent(permalink)}`);
  expect(oembedResponse.status()).toBe(200);
  const oembed = await oembedResponse.json();
  expect(oembed.html).toContain(`<iframe class="plamenu-embed" src="${embedUrl}"`);
  const embedResponse = await page.request.get(embedUrl);
  expect(embedResponse.status()).toBe(200);
  expect(embedResponse.headers()['content-security-policy']).toContain('frame-ancestors *');
  expect(embedResponse.headers()['x-frame-options']).toBeUndefined();
  expect(await embedResponse.text()).toContain('media__p2p-toggle');
  if (noScript) {
    const standaloneEmbed = await page.context().newPage();
    try {
      await standaloneEmbed.goto(embedUrl);
      const basicVideo = standaloneEmbed.locator('video').first();
      await expect(basicVideo).toBeVisible();
      await expect(standaloneEmbed.locator('.media__p2p-toggle')).toBeHidden();
      await basicVideo.evaluate(video => { video.muted = true; video.play().catch(() => {}); });
      await expect.poll(() => basicVideo.evaluate(video => video.currentTime)).toBeGreaterThan(0.1);
    } finally {
      await standaloneEmbed.close();
    }
  }
  if (!noScript && projectName === 'chromium-desktop') {
    const host = await page.context().newPage();
    const hostServer = createServer((_request, response) => {
      response.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
      response.end(`<!doctype html><html><title>Embed host</title><body>${oembed.html}</body></html>`);
    });
    await new Promise(resolve => hostServer.listen(0, '127.0.0.1', resolve));
    const trackerSockets = [];
    const peerAssets = [];
    host.on('websocket', socket => { if (socket.url().includes('/tracker/socket')) trackerSockets.push(socket.url()); });
    host.on('request', request => { if (request.url().includes('p2p-media-loader')) peerAssets.push(request.url()); });
    try {
      // A real loopback host is cross-origin but in the same private-network
      // address space; a mocked public host would be blocked by Chromium PNA.
      await host.goto(`http://127.0.0.1:${hostServer.address().port}/`);
      const frame = host.frameLocator('iframe.plamenu-embed');
      await expect(frame.locator('video[data-hls]')).toBeVisible();
      const choice = frame.getByRole('checkbox', { name: 'Share video with other viewers' });
      await expect(choice).toBeChecked();
      expect(await frame.locator('figure[data-p2p="true"]').evaluate(figure => {
        const control = figure.querySelector('.media__p2p').getBoundingClientRect();
        const video = figure.querySelector('video').getBoundingClientRect();
        return control.bottom <= video.top;
      })).toBe(true);
      await host.locator('iframe.plamenu-embed').screenshot({ path: '../target/e2e/native-video-p2p-embed-before.png' });
      await choice.focus();
      await host.keyboard.press('Space');
      await expect(choice).not.toBeChecked();
      const embeddedPlayer = frame.locator('video[data-hls]');
      await embeddedPlayer.evaluate(video => { video.muted = true; video.play().catch(() => {}); });
      await expect.poll(() => embeddedPlayer.evaluate(video => video.currentTime)).toBeGreaterThan(0.1);
      expect(trackerSockets).toEqual([]);
      expect(peerAssets).toEqual([]);
      expect(await host.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
      await host.locator('iframe.plamenu-embed').screenshot({ path: '../target/e2e/native-video-p2p-embed.png' });
      await host.locator('iframe.plamenu-embed').evaluate(element => { element.width = '320'; });
      expect(await frame.locator('body').evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
      await host.locator('iframe.plamenu-embed').screenshot({ path: '../target/e2e/native-video-p2p-embed-320.png' });
    } finally {
      await host.close();
      await new Promise(resolve => hostServer.close(resolve));
    }
  }
  await page.locator(`#post-${statusId}`).getByTitle('More options', { exact: true }).click();
  await expect(page.locator(`form[action="/web/statuses/${statusId}/redraft"]`)).toHaveCount(0);
  await page.getByRole('link', { name: 'Edit', exact: true }).click();
  const edit = page.locator('form.compose--edit');
  await expect(edit.getByLabel('Video title')).toHaveValue(`Browser video ${name}`);
  await expect(edit.locator('input[type="checkbox"][name="media_keep[]"]')).toHaveCount(0);
  await expect(edit.locator('input[type="hidden"][name="media_keep[]"]')).toHaveCount(1);
  const editedTitle = `Edited video ${name}`;
  const description = 'A detailed description with literal *asterisks*. '.repeat(20).trimEnd();
  await edit.getByLabel('Video title').fill(editedTitle);
  await edit.getByRole('textbox', { name: 'Post text', exact: true }).fill(description);
  await edit.getByRole('button', { name: 'Preview', exact: true }).click();
  await expect(page.locator('[data-compose-preview]')).toContainText(editedTitle);
  await expect(edit.getByLabel('Video title')).toHaveValue(editedTitle);
  await expect(edit.getByRole('textbox', { name: 'Post text', exact: true })).toHaveValue(description);
  const editReflow = await page.evaluate(() => ({ width: innerWidth, content: document.documentElement.scrollWidth }));
  expect(editReflow.content).toBeLessThanOrEqual(editReflow.width);
  if (!noScript) {
    const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
    expect(result.violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) }))).toEqual([]);
  }
  await edit.getByRole('button', { name: 'Save changes', exact: true }).click();
  await page.waitForURL(url => url.pathname === new URL(permalink).pathname);
  await expect(page.getByText(editedTitle, { exact: true })).toBeVisible();
  await expect(page.locator('video track[kind="captions"]').first()).toBeAttached();
  await page.goto(`/web/statuses/${statusId}/edit`);
  await expect(edit.getByLabel('Video title')).toHaveValue(editedTitle);
  await expect(edit.getByRole('textbox', { name: 'Post text', exact: true })).toHaveValue(description);
  await page.goto('/videos');
  await page.getByRole('navigation', { name: 'Your video channels', exact: true })
    .getByRole('link', { name: channelName, exact: true }).click();
  await expect(page.getByRole('heading', { level: 1 })).toContainText(channelName);
  await expect(page.getByText(editedTitle, { exact: true })).toBeVisible();
  await page.getByRole('link', { name: 'Creator profile' }).click();
  await expect(page.getByText(editedTitle, { exact: true })).toBeVisible();
  await expect(page.getByRole('link', { name: 'Hot', exact: true })).toHaveCount(0);
  await expect(page.getByText('Video channel', { exact: true })).toBeVisible();
  await expect(page.getByRole('link', { name: 'Browse videos' }))
    .toHaveAttribute('href', /\/videos\/explore\?channel=\d+/);
  await expect(page.locator('.profile__badge').filter({ hasText: /^Group$/ })).toHaveCount(0);
  await expect(page.locator('.status__boost').first()).toContainText('published on');
  await page.goto(`/web/statuses/${statusId}/edit`);
  await page.getByRole('link', {name:'Replace poster', exact:true}).click();
  const publishedPoster = page.locator('form[data-video-poster-replace]');
  await publishedPoster.getByLabel('Describe the poster image, including any text shown in it')
    .fill('Updated blue poster after publication.');
  await publishedPoster.getByLabel('Still image for the poster').setInputFiles(posterFixture);
  if (!noScript) await expect(publishedPoster.getByRole('img', {name:'Preview of the new poster image'})).toBeVisible();
  await page.setViewportSize({width:320,height:800});
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
  if (!noScript) {
    const audit = await new AxeBuilder({page}).include('#video-poster-replace').analyze();
    expect(audit.violations).toEqual([]);
  }
  await publishedPoster.getByRole('button', {name:'Save custom poster'}).press('Enter');
  await page.waitForURL(url => url.searchParams.get('saved') === '1');
  await page.goto(permalink);
  await page.getByText('Poster image description', {exact:true}).click();
  await expect(page.getByText('Updated blue poster after publication.', {exact:true})).toBeVisible();
  await expect(page.locator('video').first()).toBeVisible();
  await expect(page.locator('.media__seek-previews')).toBeVisible();
}

test('native video upload, review and publication', async ({ page }, info) => {
  test.setTimeout(180_000);
  await publish(page, info.project.name, false);
});
test('native video upload, captions and publication @no-js', async ({ page }, info) => {
  test.setTimeout(180_000);
  await publish(page, info.project.name, true);
});

async function scheduleVideo(page, noScript) {
  test.setTimeout(120_000);
  await signIn(page);
  await page.goto('/videos');
  const form = page.locator('form[action="/web/videos"]');
  const channel = form.getByRole('combobox', { name: 'Channel', exact: true });
  await channel.selectOption({ index: 0 });
  const title = `Scheduled browser film ${Date.now()}`;
  await form.getByLabel('Video title').fill(title);
  await form.getByLabel('Description', { exact: true }).fill('A planned film.');
  await form.getByLabel('Video or audio file', { exact: true }).setInputFiles(fixture);
  await form.getByRole('button', { name: 'Upload video' }).click();
  await page.waitForURL(url => /\/videos\/drafts\/\d+$/.test(url.pathname));
  const mediaId = page.url().split('/').pop();
  if (noScript) {
    for (let attempt = 0; attempt < 60; attempt++) {
      if (await page.getByRole('link', { name: 'Review your video' }).isVisible()) break;
      await page.getByRole('link', { name: 'Refresh status' }).click();
      await page.waitForTimeout(500);
    }
  }
  await page.getByRole('link', { name: 'Review your video' }).click();
  const review = page.locator('form[data-video-schedule-form]');
  const when = review.getByLabel('Publication time');
  await expect(when).toBeVisible();
  await expect(page.locator('#videos-schedule-hint')).toContainText('UTC');
  if (!noScript) {
    await review.getByRole('button', { name: 'Schedule video' }).click();
    await expect(when).toBeFocused();
    await expect(when).toHaveAttribute('required', '');
  }
  const tomorrow = new Date(Date.now() + 24 * 3600 * 1000).toISOString().slice(0, 16);
  await when.fill(tomorrow);
  if (!noScript) {
    const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
    expect(result.violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) }))).toEqual([]);
  }
  const reflow = await page.evaluate(() => ({ width: innerWidth, content: document.documentElement.scrollWidth }));
  expect(reflow.content).toBeLessThanOrEqual(reflow.width);
  await page.screenshot({ path: test.info().outputPath('scheduled-video-review.png'), fullPage: true });
  await review.getByRole('button', { name: 'Schedule video' }).click();
  await page.waitForURL(url => url.pathname === '/settings/scheduled');
  await expect(page.getByText(title, { exact: true })).toBeVisible();
  const entry = page.locator('.list-index__item').filter({ hasText: title });
  await expect(entry).toContainText('Videos');
  await entry.getByRole('button', { name: 'Cancel post', exact: true }).click();
  await expect(page.getByText(title, { exact: true })).toHaveCount(0);
  await page.goto(`/videos/drafts/${mediaId}`);
  await page.getByRole('checkbox', { name: 'I want to delete this draft and its files' }).check();
  await page.getByRole('button', { name: 'Cancel and delete draft' }).click();
  await page.waitForURL(url => url.pathname === '/videos');
}

test('native video review schedules accessibly', async ({ page }) => scheduleVideo(page, false));
test('native video review schedules without JavaScript @no-js', async ({ page }) => scheduleVideo(page, true));

async function metadataVideo(page, noScript) {
  test.setTimeout(120_000);
  await signIn(page);
  await page.goto('/videos');
  const form = page.locator('form[action="/web/videos"]');
  await expect(form).toBeVisible();
  if (await form.getByRole('combobox', { name: 'Channel', exact: true }).locator('option').count() === 0) {
    const create = page.locator('form[action="/web/video-channels"]');
    if (!await create.isVisible()) await page.locator('summary').filter({ hasText: 'Create a video channel' }).click();
    await create.getByLabel('Channel address').fill(`metadata_${Date.now()}`);
    await create.getByLabel('Channel name').fill('Metadata films');
    await create.getByRole('button', { name: 'Create a video channel' }).click();
    await page.waitForURL(url => url.pathname === '/videos');
  }
  await form.getByRole('combobox', { name: 'Channel', exact: true }).selectOption({ index: 0 });
  const title = `Metadata film ${Date.now()}`;
  await form.getByLabel('Video title').fill(title);
  await form.getByLabel('Description', { exact: true }).fill('A lesson in #astronomy.');
  await expect(form.getByRole('combobox', { name: 'Category', exact: true })).toHaveValue('0');
  await expect(form.getByRole('combobox', { name: 'Licence', exact: true })).toHaveValue('0');
  await form.getByRole('combobox', { name: 'Category', exact: true }).selectOption('13');
  await form.getByRole('combobox', { name: 'Licence', exact: true }).selectOption('2');
  if (!noScript) {
    await expect(form.locator('[data-video-licence-link]')).toHaveAttribute('href', 'https://creativecommons.org/licenses/by-sa/4.0/');
    await expect(form.locator('[data-video-licence-status]')).toBeVisible();
  }
  await form.getByLabel('Content warning', { exact: true }).fill('Flashing imagery');
  await form.getByLabel('Video or audio file', { exact: true }).setInputFiles(fixture);
  await form.getByRole('button', { name: 'Upload video' }).click();
  await page.waitForURL(url => /\/videos\/drafts\/\d+$/.test(url.pathname));
  const mediaId = page.url().split('/').pop();
  if (noScript) {
    for (let attempt = 0; attempt < 60; attempt++) {
      if (await page.getByRole('link', { name: 'Review your video' }).isVisible()) break;
      await page.getByRole('link', { name: 'Refresh status' }).click();
      await page.waitForTimeout(500);
    }
  }
  await page.getByRole('link', { name: 'Review your video' }).click();
  const review = page.locator('form[action="/web/videos"]');
  await expect(review.getByRole('combobox', { name: 'Category', exact: true })).toHaveValue('13');
  await expect(review.getByRole('combobox', { name: 'Licence', exact: true })).toHaveValue('2');
  await expect(review.locator('[data-video-licence-link]')).toHaveAttribute('href', 'https://creativecommons.org/licenses/by-sa/4.0/');
  await expect(review.locator('[data-video-licence-status]')).toBeVisible();
  await expect(review.getByLabel('Content warning', { exact: true })).toHaveValue('Flashing imagery');
  await expect(review.locator('[name="sensitive"][type="checkbox"]')).toBeChecked();
  await page.screenshot({ path: test.info().outputPath('video-metadata-review.png'), fullPage: true });
  if (!noScript) {
    const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
    expect(result.violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) }))).toEqual([]);
  }
  await review.getByRole('button', { name: 'Publish video', exact: true }).click();
  await page.waitForURL(url => /\/@[^/]+\/\d+/.test(url.pathname));
  const watchUrl = page.url();
  // The native details gate remains operable without scripts and covers media.
  const warning = page.locator('details.status__cw');
  await expect(warning).not.toHaveAttribute('open', '');
  await warning.locator(':scope > summary').press('Enter');
  await expect(page.locator('video').first()).toBeVisible();
  const metadata = page.locator('details.media__metadata');
  await metadata.locator('summary').press('Enter');
  await expect(metadata).toContainText('Education');
  await expect(metadata.getByRole('link', { name: 'CC BY-SA 4.0' })).toHaveAttribute('href', 'https://creativecommons.org/licenses/by-sa/4.0/');
  const statusId = new URL(watchUrl).pathname.split('/').pop();
  await page.goto(`/web/statuses/${statusId}/edit`);
  await page.getByRole('link', { name: 'Manage category and licence', exact: true }).click();
  await expect(page).toHaveURL(url => url.pathname === `/videos/${mediaId}/metadata`);
  await expect(page.getByRole('heading', { name: 'Manage category and licence' })).toBeVisible();
  await page.getByRole('combobox', { name: 'Category', exact: true }).selectOption('15');
  await page.getByRole('combobox', { name: 'Licence', exact: true }).selectOption('9');
  await page.screenshot({ path: test.info().outputPath('video-metadata-edit.png'), fullPage: true });
  const reflow = await page.evaluate(() => ({ width: innerWidth, content: document.documentElement.scrollWidth }));
  expect(reflow.content).toBeLessThanOrEqual(reflow.width);
  if (!noScript) {
    await expect(page.locator('[data-video-licence-status]')).toBeHidden();
    const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
    expect(result.violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) }))).toEqual([]);
  }
  await page.getByRole('button', { name: 'Save video metadata' }).click();
  await expect(page.getByRole('status').filter({ hasText: 'Video metadata saved.' })).toBeVisible();
  await expect(page.getByRole('combobox', { name: 'Category', exact: true })).toHaveValue('15');
  await expect(page.getByRole('combobox', { name: 'Licence', exact: true })).toHaveValue('9');
  await page.goto(watchUrl);
  await page.locator('details.status__cw > summary').press('Enter');
  await metadata.locator('summary').press('Enter');
  await expect(metadata).toContainText('Science and technology');
  await expect(metadata).toContainText('All rights reserved');
  await expect(metadata.locator('a')).toHaveCount(0);
  await page.goto(`/videos/${mediaId}/metadata`);
  await page.getByRole('combobox', { name: 'Category', exact: true }).selectOption('0');
  await page.getByRole('combobox', { name: 'Licence', exact: true }).selectOption('0');
  await page.getByRole('button', { name: 'Save video metadata' }).click();
  await page.goto(watchUrl);
  await expect(page.locator('details.media__metadata')).toHaveCount(0);
}

test('Video metadata upload, review, disclosure and editing', async ({ page }) => metadataVideo(page, false));
test('Video metadata authoring and editing without JavaScript @no-js', async ({ page }) => metadataVideo(page, true));

async function publishAudioOnly(page, projectName, noScript) {
  test.setTimeout(90_000);
  await signIn(page);
  await page.goto('/videos');
  const name = `audio_${Date.now().toString(36)}_${projectName.slice(0, 3)}`;
  const channelForm = page.locator('form[action="/web/video-channels"]');
  if (!await channelForm.isVisible()) {
    await page.locator('summary').filter({ hasText: 'Create a video channel' }).click();
  }
  await channelForm.getByLabel('Channel address').fill(name);
  await channelForm.getByLabel('Channel name').fill(`Films ${name}`);
  await channelForm.getByRole('button', { name: 'Create a video channel' }).click();
  await page.waitForURL(url => url.pathname === '/videos');
  const form = page.locator('form[action="/web/videos"]');
  await form.getByRole('combobox', { name: 'Channel', exact: true })
    .selectOption({ label: `Films ${name}` });
  await form.getByLabel('Video title').fill(`Waveform ${name}`);
  await form.getByLabel('Video or audio file', { exact: true }).setInputFiles(audioFixture);
  await form.getByLabel('Poster frame time (seconds)').fill('1.5');
  await form.getByLabel('Visual description').fill('A blue waveform on a dark background.');
  await form.getByLabel('Transcript', { exact: true }).fill('A steady test tone plays.');
  await form.getByRole('button', { name: 'Upload video' }).click();
  await page.waitForURL(url => /\/videos\/drafts\/\d+$/.test(url.pathname));
  const reviewLink = page.getByRole('link', { name: 'Review your video', exact: true });
  if (noScript) {
    for (let attempt = 0; attempt < 60 && !await reviewLink.isVisible(); attempt++) {
      await page.getByRole('link', { name: 'Refresh status', exact: true }).click();
      await page.waitForTimeout(500);
    }
  }
  await expect(reviewLink).toBeVisible({ timeout: 45_000 });
  await reviewLink.click();
  const review = page.locator('form[action="/web/videos"]');
  await expect(page.getByRole('heading', { name: 'Review your video' })).toBeVisible();
  await expect(page.locator('video source')).toHaveAttribute('src', /\/preview\/video$/);
  await expect(page.getByRole('img', { name: 'Selected poster frame' })).toBeVisible();
  await page.locator('details').filter({ has: page.locator('summary', { hasText: 'Transcript' }) })
    .locator('summary').click();
  await expect(page.getByText('A steady test tone plays.', { exact: true })).toBeVisible();
  if (!noScript) {
    const axe = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
    expect(axe.violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) }))).toEqual([]);
  }
  await review.getByRole('button', { name: 'Publish video', exact: true }).click();
  await page.waitForURL(url => /\/@[^/]+\/\d+/.test(url.pathname));
  const player = page.locator('video').first();
  await expect(player).toBeVisible();
  await page.locator('details.media__transcript summary').click();
  await expect(page.getByText('A steady test tone plays.', { exact: true })).toBeVisible();
  const alt = page.getByRole('button', { name: 'ALT: A blue waveform on a dark background.', exact: true });
  await alt.press('Enter');
  await expect(page.locator('.media__alt-text').filter({ hasText: 'A blue waveform on a dark background.' }))
    .toBeVisible();
  const master = await page.request.get(await player.getAttribute('data-hls'));
  expect(master.ok()).toBeTruthy();
  expect(await master.text()).toContain('RESOLUTION=640x360');
  await player.evaluate(video => { video.muted = true; video.play().catch(() => {}); });
  await expect.poll(() => player.evaluate(video => video.videoWidth)).toBe(640);
  await expect.poll(() => player.evaluate(video => video.currentTime)).toBeGreaterThan(0.1);
  expect(await player.evaluate(video => video.error)).toBeNull();
  await player.evaluate(video => video.pause());
  const reflow = await page.evaluate(() => ({ width: innerWidth, content: document.documentElement.scrollWidth }));
  expect(reflow.content).toBeLessThanOrEqual(reflow.width);
}

test('audio-only Video source publishes as a waveform Video', async ({ page }, info) => {
  await publishAudioOnly(page, info.project.name, false);
});
test('audio-only Video source is authorable and playable @no-js', async ({ page }, info) => {
  await publishAudioOnly(page, info.project.name, true);
});

test('interrupted video upload resumes from its committed offset', async ({ page }, info) => {
  test.setTimeout(90_000);
  await signIn(page);
  await page.goto('/videos');
  const name = `resume_${Date.now().toString(36)}_${info.project.name.slice(0, 3)}`;
  const channelForm = page.locator('form[action="/web/video-channels"]');
  if (!await channelForm.isVisible()) await page.locator('summary').filter({ hasText: 'Create a video channel' }).click();
  await channelForm.getByLabel('Channel address').fill(name);
  await channelForm.getByLabel('Channel name').fill(`Films ${name}`);
  await channelForm.getByRole('button', { name: 'Create a video channel' }).click();
  await page.waitForURL(url => url.pathname === '/videos');
  const form = page.locator('form[action="/web/videos"]');
  await form.getByRole('combobox', { name: 'Channel', exact: true }).selectOption({ label: `Films ${name}` });
  await form.getByLabel('Video title').fill(`Resume ${name}`);
  const source = { name: 'resumable.mp4', mimeType: 'video/mp4', buffer: Buffer.alloc(8 * 1024 * 1024 + 4096, 7) };
  await form.getByLabel('Video or audio file', { exact: true }).setInputFiles(source);
  let chunks = 0;
  await page.route('**/web/videos/drafts/*/source', route => {
    if (route.request().method() !== 'PATCH' || ++chunks === 1) return route.continue();
    return route.abort('failed');
  });
  await form.getByRole('button', { name: 'Upload video' }).click();
  const link = form.getByRole('link', { name: 'Open your private draft to resume.' });
  await expect(link).toBeVisible();
  const draftUrl = new URL(await link.getAttribute('href'), page.url());
  const id = draftUrl.pathname.split('/').pop();
  const before = await (await page.request.get(`/web/videos/drafts/${id}/source`)).json();
  expect(before.offset).toBe(8 * 1024 * 1024);
  expect(before.total).toBe(source.buffer.length);
  await page.unroute('**/web/videos/drafts/*/source');
  await link.click();
  await expect(page.getByRole('heading', { name: 'Video draft' })).toBeVisible();
  const resume = page.locator('form[data-video-resume]');
  await expect(resume).toBeVisible();
  await resume.getByLabel('Select the same video or audio file').setInputFiles({ name: 'other.mp4', mimeType: 'video/mp4', buffer: Buffer.from('wrong') });
  await resume.getByRole('button', { name: 'Resume upload' }).click();
  await expect(resume.getByRole('alert')).toContainText('This is not the same file');
  await resume.getByLabel('Select the same video or audio file').setInputFiles(source);
  await resume.getByRole('button', { name: 'Resume upload' }).click();
  await page.waitForURL(url => url.pathname === draftUrl.pathname);
  await expect(resume).toHaveCount(0);
  const phase = await (await page.request.get(`/videos/drafts/${id}/status`)).json();
  expect(phase.phase).not.toBe('receiving');
  expect((await page.request.get(`/media/${id}.orig`)).status()).toBe(404);
  const reflow = await page.evaluate(() => ({ width: innerWidth, content: document.documentElement.scrollWidth }));
  expect(reflow.content).toBeLessThanOrEqual(reflow.width);
  const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
  expect(result.violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) }))).toEqual([]);
});
