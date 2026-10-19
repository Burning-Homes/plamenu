import { test, expect } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import { randomUUID } from 'node:crypto';
import { signIn, openVideoOptions, createVideoChannel ,videoChannelForm,openDetailsFor,chooseVideoLanguage} from '../session.mjs';

async function scan(page, noScript) {
  const reflow = await page.evaluate(() => ({ width:document.documentElement.clientWidth, content:document.documentElement.scrollWidth,
    outside:[...document.querySelectorAll('*')].filter(e=>e.getBoundingClientRect().right>innerWidth+1 || e.scrollWidth>e.clientWidth+5).slice(0,20).map(e=>({tag:e.tagName,type:e.type,text:e.tagName==='LABEL'?e.textContent.slice(0,80):undefined,class:String(e.className),overflow:getComputedStyle(e).overflow,right:e.getBoundingClientRect().right,width:e.getBoundingClientRect().width,scroll:e.scrollWidth,client:e.clientWidth})) }));
  expect(reflow.content, JSON.stringify(reflow)).toBeLessThanOrEqual(reflow.width);
  if (!noScript) {
    const audit = new AxeBuilder({ page }).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']);
    // Forced system colors are painted differently from authored RGB values.
    // Inspect that appearance separately; retain normal-theme contrast checks.
    if (await page.evaluate(()=>matchMedia('(forced-colors: active)').matches)) audit.disableRules(['color-contrast']);
    const report = await audit.analyze();
    expect(report.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
}
const caption = text => ({ name:'track.vtt', mimeType:'text/vtt', buffer:Buffer.from(`WEBVTT\n\n00:00:00.000 --> 00:00:02.800\n${text}\n\n00:00:01.000 --> 00:00:02.800\nClosing cue\n`) });

async function journey(page, info) {
  test.setTimeout(180_000);
  const noScript = info.project.use.javaScriptEnabled === false;
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  await signIn(page);
  await page.goto('/videos/upload');await openVideoOptions(page);
  const name = `alt_${randomUUID().replaceAll('-','').slice(0,20)}`;
  const channel = await videoChannelForm(page);
  if (!await channel.isVisible()) await channel.locator('..').locator(':scope > summary').press('Enter');
  await channel.getByLabel('Channel address').fill(name);
  await channel.getByLabel('Channel name').fill(`Alternatives ${name}`);
  await createVideoChannel(page, channel.getByRole('button', { name:'Create a video channel' }));
  await page.waitForURL(url=>url.pathname === '/videos/upload');await openVideoOptions(page);
  const upload = page.locator('form[action="/web/videos"]');
  await upload.getByRole('combobox', { name:'Channel', exact:true }).selectOption({ label:`Alternatives ${name}` });
  await upload.getByLabel('Video title').fill(`Accessible film ${name}`);
  await chooseVideoLanguage(upload,'en');
  await upload.getByLabel('Visual description').fill('A changing colour pattern.');
  await upload.getByLabel('Transcript', { exact:true }).fill('An audible tone accompanies a changing pattern.');
  await upload.getByRole('combobox', { name:'Access to important visuals' }).selectOption('audio_description');
  await upload.getByLabel('Video or audio file', { exact:true }).setInputFiles(process.env.A11Y_VIDEO_FIXTURE);
  await upload.getByLabel('Captions (WebVTT)').setInputFiles(caption('[Steady tone]'));
  await scan(page,noScript);
  await upload.getByRole('button', { name:'Upload video', exact:true }).press('Enter');
  await page.waitForURL(url=>/\/videos\/drafts\/\d+$/.test(url.pathname));
  const mediaId = new URL(page.url()).pathname.split('/').pop();
  const review = page.getByRole('link', { name:'Review your video', exact:true });
  if (noScript) {
    for (let i=0;i<80 && !await review.isVisible();i++) {
      await page.getByRole('link', { name:'Refresh status', exact:true }).press('Enter');
      await page.waitForTimeout(500);
    }
  }
  await expect(review).toBeVisible({timeout:60_000});
  await review.press('Enter');
  const chapterForm = page.locator('form[data-chapter-editor]');
  await openDetailsFor(chapterForm);
  await chapterForm.locator('textarea').fill('0:00 Opening pattern\n0:01 Closing pattern');
  await chapterForm.getByRole('button', { name:'Save chapters' }).press('Enter');
  await page.locator('form[action="/web/videos"]').getByRole('button', { name:'Publish video', exact:true }).press('Enter');
  await page.waitForURL(url=>/\/@[^/]+\/\d+$/.test(url.pathname));
  const watch = page.url();
  const statusId = new URL(watch).pathname.split('/').pop();
  await page.locator('summary').filter({hasText:/^Audio and visual alternatives$/}).press('Enter');
  await expect(page.locator('.media__alternatives').getByText('This video includes audio description', {exact:true})).toBeVisible();
  await page.goto(`/videos/${mediaId}/captions`);
  if (!noScript) {
    const form=page.locator('form[data-caption-upload]');
    await form.getByLabel('Captions (WebVTT)').setInputFiles(caption('A valid caption with missing language'));
    await form.getByRole('button',{name:'Add or replace captions'}).press('Enter');
    await expect(form.locator('[data-combo-validation]')).toBeVisible();
    await expect(form.locator('[data-combo-trigger]')).toBeFocused();
    await expect(form.locator('select[name="language"]')).toHaveValue('');
    await page.screenshot({path:info.outputPath('caption-required-picker.png'),fullPage:false});
  }
  if (noScript) {
    const form=page.locator('form[data-caption-upload]');
    await chooseVideoLanguage(form,'fr');
    await form.getByLabel('Captions (WebVTT)').setInputFiles({name:'oversized.vtt',mimeType:'text/vtt',buffer:Buffer.alloc(270000,65)});
    const rejected=page.waitForResponse(r=>r.request().method()==='POST'&&new URL(r.url()).pathname===`/web/videos/${mediaId}/captions`);
    await form.getByRole('button',{name:'Add or replace captions'}).press('Enter');
    expect((await rejected).status()).toBe(422);
    await expect(page.getByRole('alert')).toContainText('256 KiB');
    await expect(form.locator('select[name="language"]')).toHaveValue('fr');
    await expect(form.locator('select[name="language"] option:checked')).toContainText('French');
    await page.screenshot({path:info.outputPath('caption-nojs-error-retains-french.png'),fullPage:true});
  }
  async function french(text) {
    const form = page.locator('form[data-caption-upload]');
    await chooseVideoLanguage(form,'fr');
    await form.getByLabel('Captions (WebVTT)').setInputFiles(caption(text));
    await form.getByRole('button', {name:'Add or replace captions'}).press('Enter');
    await expect(page.locator('video track[srclang="fr"]')).toHaveCount(1);
  }
  await french('Bonjour initial');
  const oldTrack = await page.locator('video track[srclang="fr"]').getAttribute('src');
  await french('Bonjour corrigé');
  const track = await page.locator('video track[srclang="fr"]').getAttribute('src');
  expect(track).not.toBe(oldTrack);
  expect((await page.request.get(oldTrack)).status()).toBe(404);
  await page.locator('form[action$="/captions/en/remove"] button').press('Enter');
  await expect(page.locator('video track[srclang="en"]')).toHaveCount(0);
  await page.goto(watch);
  const player = page.locator('video').first();
  await player.evaluate(video=>{
    video.muted=true;
    for (const track of video.textTracks) track.mode=track.language==='fr'?'showing':'disabled';
    video.play().catch(()=>{});
  });
  await expect.poll(()=>player.evaluate(v=>[...v.textTracks].find(t=>t.language==='fr')?.activeCues?.[0]?.text), {timeout:30_000}).toBe('Bonjour corrigé');
  await expect.poll(()=>player.evaluate(v=>v.getVideoPlaybackQuality().totalVideoFrames), {timeout:30_000}).toBeGreaterThan(0);
  await player.evaluate(v=>v.pause());
  expect(await player.evaluate(v=>v.getVideoPlaybackQuality().totalVideoFrames)).toBeGreaterThan(0);
  await page.locator('summary').filter({hasText:/^Read captions as text$/}).press('Enter');
  await page.locator('.media__caption-transcripts a').filter({hasText:'fr'}).press('Enter');
  await expect(page.locator('.caption-transcript')).toContainText('Bonjour corrigé');
  const alternatives = page.locator('summary').filter({hasText:/^Transcript and visual description$/});
  await alternatives.press('Enter');
  await expect(page.getByText('An audible tone accompanies a changing pattern.', {exact:true})).toBeVisible();
  const seek = page.locator('a[data-transcript-seek-ms="1000"]');
  if (noScript) expect(await seek.getAttribute('href')).toMatch(/#t=1\.000$/);
  else {
    await seek.press('Enter');
    await expect.poll(()=>page.locator('.caption-transcript video').evaluate(v=>v.currentTime), {timeout:15_000}).toBeGreaterThanOrEqual(.9);
    await expect(page).toHaveURL(/\/transcript\/fr$/);
  }
  await page.setViewportSize({width:320,height:900});
  await scan(page,noScript);
  await page.screenshot({path:info.outputPath('caption-transcript-mobile.png'),fullPage:true});
  await page.goto(`/web/statuses/${statusId}/edit`);
  await page.getByLabel('Alt text', {exact:true}).fill('Updated scene <not HTML>.');
  await page.getByRole('textbox', {name:'Transcript or media alternative',exact:true}).fill('Revised written alternative <script>literal</script>.');
  await page.getByRole('combobox', {name:'Access to important visuals'}).selectOption('soundtrack');
  await page.getByRole('button', {name:'Save changes',exact:true}).press('Enter');
  await page.locator('summary').filter({hasText:/^Transcript and visual description$/}).press('Enter');
  await expect(page.getByText('Revised written alternative <script>literal</script>.',{exact:true})).toBeVisible();
  await page.locator('summary').filter({hasText:/^Audio and visual alternatives$/}).press('Enter');
  await expect(page.locator('.media__alternatives').getByText('The soundtrack already conveys every important visual',{exact:true})).toBeVisible();
  const value = await (await page.request.get(`/api/v1/statuses/${statusId}`)).json();
  expect(value.media_attachments[0].audio_described).toBe(false);
  expect(value.media_attachments[0].visuals_conveyed_in_audio).toBe(true);
  expect(value.media_attachments[0].captions.map(c=>c.language)).toEqual(['fr']);
  expect(value.media_attachments[0].chapters.map(c=>c.start_seconds)).toEqual([0,1]);
  await scan(page,noScript);
  await page.emulateMedia({reducedMotion:'reduce',forcedColors:'active'});
  await scan(page,noScript);
  await page.screenshot({path:info.outputPath('alternatives-forced-colours.png'),fullPage:true});
  await page.goto(`/videos/${mediaId}/chapters`);
  await page.locator('form[data-chapter-editor] textarea').fill('');
  const removedChapters=page.waitForResponse(r=>r.request().method()==='POST'&&new URL(r.url()).pathname===`/web/videos/${mediaId}/chapters`);
  await page.getByRole('button',{name:'Save chapters'}).press('Enter');
  expect((await removedChapters).status()).toBe(303);
  await page.goto(`/videos/${mediaId}/captions`);
  await page.locator('form[action$="/captions/fr/remove"] button').press('Enter');
  await expect(page.locator('video track[srclang="fr"]')).toHaveCount(0);
  expect((await page.request.get(track)).status()).toBe(404);
  expect((await page.request.get(`/videos/${mediaId}/transcript/fr`)).status()).toBe(404);
  await page.goto(watch);
  // Empty optional metadata is omitted from the client entity.
  expect((await (await page.request.get(`/api/v1/statuses/${statusId}`)).json()).media_attachments[0].chapters ?? []).toEqual([]);
  await expect(page.locator('[data-chapter-seek-ms]')).toHaveCount(0);
  await expect(page.locator('video track')).toHaveCount(0);
  expect(errors).toEqual([]);
}
test('accessible Video metadata lifecycle', async ({page},info)=>journey(page,info));
test('accessible Video metadata lifecycle @no-js', async ({page},info)=>journey(page,info));
