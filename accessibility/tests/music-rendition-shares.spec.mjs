import {playNativeAudio} from '../session.mjs';
import {freshMusicAudio} from '../session.mjs';
import {test,expect} from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import {signIn,openDetailsFor,openMusicUpload} from '../session.mjs';
async function journey(page,browser,info,noScript){
  test.setTimeout(180000);
  await signIn(page);await openMusicUpload(page);
  const upload=page.locator('form[action="/web/music/assets/import"]');await openDetailsFor(upload);
  await upload.getByLabel('Audio file',{exact:true}).setInputFiles(await freshMusicAudio());
  await upload.getByRole('button',{name:'Save privately',exact:true}).press('Enter');
  await page.waitForURL(url=>/^\/music\/assets\/\d+$/.test(url.pathname));
  const id=new URL(page.url()).pathname.split('/').at(-1);
  await page.getByRole('link',{name:'Create share link',exact:true}).press('Enter');
  await page.getByRole('button',{name:'Create share link',exact:true}).press('Enter');
  const shared=await page.locator('#music-share-url').inputValue();
  const context=await browser.newContext({baseURL:process.env.A11Y_BASE_URL,ignoreHTTPSErrors:false,javaScriptEnabled:!noScript,extraHTTPHeaders:{},viewport:{width:320,height:800}});
  try{
    const guest=await context.newPage();await guest.goto(shared);
    await guest.getByRole('link',{name:'Playback formats',exact:true}).press('Enter');
    const formats=guest.url();
    await expect(guest.locator('input[name="csrf"]')).toHaveCount(0);
    const form=guest.locator(`form[action="/web/music/assets/${id}/renditions"]`);
    await expect(form.locator('input[name="token"]')).toHaveValue(new URL(shared).searchParams.get('token'));
    await guest.screenshot({path:info.outputPath('music-shared-playback-formats.png'),fullPage:true});
    for(const format of ['flac','mp3']){
      await guest.goto(formats);
      await form.getByLabel('Audio format',{exact:true}).selectOption(format);
      await form.getByLabel('Bitrate (kbps)',{exact:true}).fill('320');
      await Promise.all([guest.waitForURL(url=>url.pathname===new URL(formats).pathname&&url.searchParams.has('job')),form.getByRole('button',{name:'Prepare playback',exact:true}).press('Enter')]);
      await expect.poll(async()=>{await guest.reload();return await guest.locator('[data-rendition-state]').getAttribute('data-rendition-state');},{timeout:60000}).toBe('ready');
      await expect(guest.locator('[data-actual-bitrate]')).toHaveAttribute('data-actual-bitrate',format==='flac'?'lossless':'192');
      const src=await guest.locator('audio').getAttribute('src');
      expect(new URL(src,guest.url()).pathname).toMatch(/\/music\/share\/assets\/\d+\/renditions\/\d+\/stream$/);
      const response=await context.request.get(src,{headers:{Range:'bytes=0-31'}});
      expect(response.status()).toBe(206);expect(response.headers()['referrer-policy']).toBe('no-referrer');
      expect((await context.request.get(src.replace('/share/','/'),{headers:{Range:'bytes=0-31'}})).status()).toBe(404);
      expect(await guest.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
      if(!noScript){
        const result=await new AxeBuilder({page:guest}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();expect(result.violations).toEqual([]);
      }
      await guest.screenshot({path:info.outputPath(`music-shared-ready-${format}.png`),fullPage:true});
      if(format==='mp3'&&!noScript){
        await playNativeAudio(guest.locator('audio'),info);await expect.poll(()=>guest.locator('audio').evaluate(a=>a.currentTime),{timeout:15000}).toBeGreaterThan(1);
      }
    }
    const player=guest.url();const stream=await guest.locator('audio').getAttribute('src');
    await page.goto('/music/shares');
    const revoke=page.locator('form[action="/web/music/shares/revoke"]').filter({has:page.locator(`input[name="id"][value="${id}"]`)});
    await Promise.all([page.waitForNavigation({waitUntil:'domcontentloaded'}),revoke.getByRole('button',{name:'Revoke access',exact:true}).press('Enter')]);
    expect((await context.request.get(stream,{headers:{Range:'bytes=0-31','If-None-Match':'*'}})).status()).toBe(404);
    expect((await context.request.get(player)).status()).toBe(404);
  }finally{await context.close();}
}
test('shared private music prepares scoped native formats',async({page,browser},info)=>journey(page,browser,info,false));
test('shared format controls work without JavaScript @no-js',async({page,browser},info)=>journey(page,browser,info,true));
