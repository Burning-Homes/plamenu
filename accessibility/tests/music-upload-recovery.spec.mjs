import {test,expect} from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import {signIn,freshMusicAudio,playNativeAudio} from '../session.mjs';

async function journey(page,info) {
  test.setTimeout(180000);
  if(info.project.use.javaScriptEnabled===false) await page.setViewportSize({width:320,height:800});
  await signIn(page);
  await page.goto('/music/assets?upload=true');
  const form=page.locator('form.music-upload');
  const title=`Retained music ${info.project.name}`;
  const artist='Artist <safe> & friends';
  const album='Recovery sessions';
  await form.locator('[name=title]').fill(title);
  await form.locator('summary').press('Enter');
  await form.locator('[name=artist]').fill(artist);
  await form.locator('[name=album]').fill(album);
  await form.locator('[name=file]').setInputFiles({name:'invalid.wav',mimeType:'audio/wav',buffer:Buffer.from('This is not an audio recording.')});
  const rejected=page.waitForResponse(r=>r.request().method()==='POST'&&new URL(r.url()).pathname==='/web/music/assets/import');
  await form.locator('button[value=publish]').press('Enter');
  expect((await rejected).status()).toBe(422);
  await expect(page.getByRole('alert')).toContainText('supported audio recording');
  await page.screenshot({path:info.outputPath('music-upload-error.png'),fullPage:true});
  await expect.soft(page.locator('#music-upload-error')).toBeFocused({timeout:1000});
  await expect.soft(form.locator('[name=title]')).toHaveValue(title,{timeout:1000});
  await expect.soft(form.locator('[name=artist]')).toHaveValue(artist,{timeout:1000});
  await expect.soft(form.locator('[name=album]')).toHaveValue(album,{timeout:1000});
  await expect.soft(form.locator('[name=artist]')).toBeVisible({timeout:1000});
  if(await form.count()) {
    await expect(form.locator('[name=file]')).toHaveValue('');
    await expect(form.locator('[name=file]')).toHaveAttribute('required','');
    expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
    if(info.project.use.javaScriptEnabled!==false) {
      const scan=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
      expect.soft(scan.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
    }
  } else {
    // Baseline can still finish the real correction path after recording lost work.
    await page.getByRole('link',{name:'Upload audio',exact:true}).press('Enter');
    await form.locator('[name=title]').fill(title);
    await form.locator('summary').press('Enter');
    await form.locator('[name=artist]').fill(artist);
    await form.locator('[name=album]').fill(album);
  }
  await form.locator('[name=file]').setInputFiles(await freshMusicAudio());
  await form.locator('button[value=publish]').press('Enter');
  await expect(page).toHaveURL(url=>/^\/music\/assets\/\d+$/.test(url.pathname));
  await expect(page.getByRole('heading',{name:title,exact:true})).toBeVisible();
  await expect(page.locator('.music-library')).toContainText(artist);
  await expect(page.locator('.music-library')).toContainText(album);
  await playNativeAudio(page.locator('audio').first(),info);
  await expect.poll(()=>page.locator('audio').first().evaluate(a=>a.currentTime)).toBeGreaterThan(0);
}

test('audio upload rejection retains metadata through correction and publication',async({page},info)=>journey(page,info));
test('@no-js audio upload rejection retains metadata through correction and publication',async({page},info)=>journey(page,info));
