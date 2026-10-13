import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {signIn,freshMusicAudio,playNativeAudio} from '../session.mjs';

async function journey(page,info,noScript) {
  test.setTimeout(180000);
  await signIn(page);
  const widths=info.project.name.includes('mobile')?[320,390]:[768,1440];
  for (const width of widths) {
    await page.setViewportSize({width,height:900});
    await page.goto('/compose');
    const kind=page.locator('[data-compose-kind]');
    expect(await kind.locator('option').evaluateAll(es=>es.map(e=>e.value))).toEqual(['note','article','event','audio','video','live']);
    await expect(page.locator('.media-create-nav')).toHaveCount(0);
    await kind.selectOption('video');
    if(noScript) {
      await page.getByRole('button',{name:'Change type',exact:true}).click();
      await page.locator('[data-compose-workflow-link]').click();
    }
    await expect(page).toHaveURL(/\/videos\/upload$/);
    await page.screenshot({path:info.outputPath(`upload-${width}.png`),fullPage:true,animations:'disabled',timeout:30000});
    await page.locator('.compose-kind-menu > summary').click();
    await page.getByRole('navigation',{name:'Create and share'}).getByRole('link',{name:'Go live',exact:true}).click();
    await expect(page).toHaveURL(/\/videos\/live\/new$/);
    const channel=page.locator('form[action="/web/video-channels"]');
    if (await channel.count()) {
      await expect(channel).toBeVisible();
      await expect(channel.locator('[name=return_to]')).toHaveValue('/videos/live/new');
      await page.screenshot({path:info.outputPath(`live-first-channel-${width}.png`),fullPage:true,animations:'disabled',timeout:30000});
      const name='livejourney_'+Date.now().toString(36);
      await channel.getByLabel('Channel address').fill(name);
      await channel.getByLabel('Channel name').fill('Evening sessions');
      await channel.getByRole('button',{name:'Create a video channel',exact:true}).click();
      await expect(page.locator('form[action="/web/videos/live"]')).toBeVisible();
      await expect(page).toHaveURL(/\/videos\/live\/new$/);
    }
    await expect(page.getByRole('button',{name:'Continue to camera setup'})).toBeVisible();
    await page.screenshot({path:info.outputPath(`live-setup-${width}.png`),fullPage:true,animations:'disabled',timeout:30000});
    await page.locator('.compose-kind-menu > summary').click();
    await page.getByRole('navigation',{name:'Create and share'}).getByRole('link',{name:'Audio',exact:true}).click();
    await page.getByLabel('Audio title',{exact:true}).fill('Evening sessions');
    await expect(page.getByText('More music details (optional)',{exact:true})).toBeVisible();
    await expect(page.getByLabel('MusicBrainz recording ID',{exact:true})).toBeHidden();
    if (!noScript) await expect(page.getByRole('button',{name:'Choose an audio file Click, drag, or paste',exact:true})).toBeVisible();
    await page.screenshot({path:info.outputPath(`audio-compose-${width}.png`),fullPage:true,animations:'disabled',timeout:30000});
    if (!noScript) {
      const picker=page.locator('.compose__media-body > input[type=file]');
      await expect(picker).toHaveAttribute('accept','audio/*');
      expect(await picker.evaluate(el=>el.multiple)).toBe(false);
      await picker.setInputFiles({name:'wrong-video.mp4',mimeType:'video/mp4',buffer:Buffer.from('invalid choice')});
      await expect(page.getByRole('alert')).toContainText('Choose one audio file');
      await expect(page.locator('.compose__attachment')).toHaveCount(0);
      await picker.setInputFiles(await freshMusicAudio());
      await expect(page.locator('.compose__attachment')).toHaveCount(1);
      await expect(page.getByRole('alert')).toBeHidden();
      const audioPreview=page.locator('.compose__attachment audio');
      await playNativeAudio(audioPreview,info);
      await expect.poll(()=>audioPreview.evaluate(a=>a.currentTime)).toBeGreaterThan(0);
      await audioPreview.evaluate(a=>a.pause());
      await page.screenshot({path:info.outputPath(`audio-file-selected-${width}.png`),fullPage:true,animations:'disabled',timeout:30000});
    } else {
      await expect(page.locator('input[name="media[]"]')).toHaveCount(1);
      await expect(page.locator('input[name="media[]"]')).toHaveAttribute('accept','audio/*');
    }
    expect(await page.evaluate(()=>document.documentElement.scrollWidth)).toBeLessThanOrEqual(width);
    if (!noScript) {
      const audit=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
      expect(audit.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
    }
    await page.goto('/videos/explore');
    await expect(page.getByRole('link',{name:'Go live',exact:true})).toBeVisible();
    await page.screenshot({path:info.outputPath(`watch-${width}.png`),fullPage:true,animations:'disabled',timeout:30000});
  }
}
test('media creation is reachable from posting and keeps the first live setup journey',async({page},info)=>journey(page,info,false));
test('media creation keeps the same journey without scripts @no-js',async({page},info)=>journey(page,info,true));
