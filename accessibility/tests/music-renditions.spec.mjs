import {playNativeAudio} from '../session.mjs';
import {freshMusicAudio} from '../session.mjs';
import {test,expect} from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import {signIn,openDetailsFor,openMusicUpload} from '../session.mjs';

async function audit(page,noScript){
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
  if(!noScript){
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
    expect(result.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
}
async function journey(page,browser,info,noScript){
  test.setTimeout(180000);
  await signIn(page);
  await openMusicUpload(page);
  const upload=page.locator('form[action="/web/music/assets/import"]');
  await openDetailsFor(upload);
  await upload.getByLabel('Audio file',{exact:true}).setInputFiles(await freshMusicAudio());
  await upload.getByRole('button',{name:'Save privately',exact:true}).press('Enter');
  await page.waitForURL(url=>/^\/music\/assets\/\d+$/.test(url.pathname));
  const track=page.url();const id=new URL(track).pathname.split('/').at(-1);
  await openDetailsFor(page.getByRole('link',{name:'Playback formats',exact:true,includeHidden:true}));
  await page.getByRole('link',{name:'Playback formats',exact:true}).press('Enter');
  const formats=page.url();
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('music-playback-formats.png'),fullPage:true});
  for(const format of ['mp3','ogg','flac','aac','opus']){
    await page.goto(formats);
    const form=page.locator(`form[action="/web/music/assets/${id}/renditions"]`);
    await form.getByLabel('Audio format',{exact:true}).selectOption(format);
    await form.getByLabel('Bitrate (kbps)',{exact:true}).fill('320');
    await Promise.all([page.waitForURL(url=>url.pathname===new URL(formats).pathname&&url.searchParams.has('job')),form.getByRole('button',{name:'Prepare playback',exact:true}).press('Enter')]);
    await expect.poll(async()=>{
      await page.reload();return await page.locator('[data-rendition-state]').getAttribute('data-rendition-state');
    },{timeout:60000}).toBe('ready');
    await expect(page.locator('audio')).toHaveCount(1);
    const src=await page.locator('audio').getAttribute('src');
    const response=await page.request.get(src,{headers:{Range:'bytes=0-31'}});
    expect(response.status()).toBe(206);
    expect(response.headers()['content-type']).toBe({mp3:'audio/mpeg',ogg:'audio/ogg',flac:'audio/flac',aac:'audio/mp4',opus:'audio/opus'}[format]);
    await expect(page.locator('[data-actual-bitrate]')).toHaveAttribute('data-actual-bitrate',format==='flac'?'lossless':'192');
    if(format==='mp3'){
      await audit(page,noScript);
      await page.screenshot({path:info.outputPath('music-ready-playback-format.png'),fullPage:true});
      if(!noScript){
        await playNativeAudio(page.locator('audio'),info);
        await expect.poll(()=>page.locator('audio').evaluate(a=>a.currentTime),{timeout:15000}).toBeGreaterThan(1);
      }
    }
  }
  await page.goto(track);
  const edit=page.locator(`form[action="/web/music/assets/${id}/edit"]`);
  await openDetailsFor(edit);
  await edit.getByLabel('Who can listen',{exact:true}).selectOption('public');
  await Promise.all([page.waitForNavigation({waitUntil:'domcontentloaded'}),edit.getByRole('button',{name:'Save track',exact:true}).press('Enter')]);
  const external=await browser.newContext({baseURL:process.env.A11Y_BASE_URL,ignoreHTTPSErrors:false,javaScriptEnabled:!noScript,extraHTTPHeaders:{}});
  try{
    const guest=await external.newPage();await guest.goto(formats);
    const listen=guest.getByRole('link',{name:'Listen',exact:true}).first();
    await expect(listen).toBeVisible();await listen.press('Enter');
    const src=await guest.locator('audio').getAttribute('src');
    expect((await external.request.get(src,{headers:{Range:'bytes=0-31'}})).status()).toBe(206);
    await page.goto(track);await openDetailsFor(edit);
    await edit.getByLabel('Who can listen',{exact:true}).selectOption('private');
    await Promise.all([page.waitForNavigation({waitUntil:'domcontentloaded'}),edit.getByRole('button',{name:'Save track',exact:true}).press('Enter')]);
    expect((await external.request.get(src,{headers:{Range:'bytes=0-31','If-None-Match':'*'}})).status()).toBe(404);
    expect((await external.request.get(formats)).status()).toBe(404);
  }finally{await external.close();}
  await page.goto(formats);
  await Promise.all([page.waitForNavigation({waitUntil:'domcontentloaded'}),page.getByRole('button',{name:'Remove cached format',exact:true}).first().press('Enter')]);
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('music-cached-playback-formats.png'),fullPage:true});
}
test('native music formats encode and respect current byte access',async({page,browser},info)=>journey(page,browser,info,false));
test('native music format controls work without JavaScript @no-js',async({page,browser},info)=>journey(page,browser,info,true));
