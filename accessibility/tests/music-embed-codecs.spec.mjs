import {playNativeAudio} from '../session.mjs';
import {freshMusicAudio} from '../session.mjs';
import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {signIn,openDetailsFor} from '../session.mjs';
import {createServer} from 'node:http';
async function journey(page,browser,info,noScript){
  test.setTimeout(180000);
  await signIn(page);await page.goto('/music/assets?mine=true&upload=true');
  const upload=page.locator('form[action="/web/music/assets/import"]');await openDetailsFor(upload);
  await upload.getByLabel('Audio file',{exact:true}).setInputFiles(await freshMusicAudio());
  await upload.getByRole('button',{name:'Save privately',exact:true}).press('Enter');
  await page.waitForURL(url=>/^\/music\/assets\/\d+$/.test(url.pathname));
  const id=new URL(page.url()).pathname.split('/').at(-1);
  const edit=page.locator(`form[action="/web/music/assets/${id}/edit"]`);await openDetailsFor(edit);
  await edit.getByLabel('Who can listen',{exact:true}).selectOption('public');await Promise.all([page.waitForNavigation({waitUntil:'domcontentloaded'}),edit.getByRole('button',{name:'Save track',exact:true}).press('Enter')]);
  await openDetailsFor(page.getByRole('link',{name:'Embed music',exact:true,includeHidden:true}));
  await page.getByRole('link',{name:'Embed music',exact:true}).press('Enter');
  const form=page.locator('form[method="get"]').filter({has:page.locator('#music-embed-format')});
  await form.locator('#music-embed-format').selectOption(noScript?'iframe':'script');
  await form.getByLabel('Native track audio format',{exact:true}).selectOption('opus');
  await form.getByLabel('Bitrate (kbps)',{exact:true}).fill('320');
  await form.getByRole('button',{name:'Show embed code',exact:true}).press('Enter');
  const snippet=await page.locator('#music-embed-code').inputValue();
  expect(snippet).not.toContain('bitrate=320');expect(snippet).not.toContain('data-bitrate="320"');
  await page.screenshot({path:info.outputPath('music-embed-codec-controls.png'),fullPage:true});
  const publisher=createServer((_request,response)=>{response.writeHead(200,{'Content-Type':'text/html; charset=utf-8'});response.end(`<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>Music codec publisher</title><style>body{font:16px system-ui;margin:12px}iframe{border:0;width:100%;max-width:100%}</style><h1>Music codec publisher</h1>${snippet}</html>`);});
  await new Promise(resolve=>publisher.listen(0,'127.0.0.1',resolve));
  const context=await browser.newContext({baseURL:process.env.A11Y_BASE_URL,ignoreHTTPSErrors:false,javaScriptEnabled:!noScript,extraHTTPHeaders:{},viewport:{width:320,height:800}});
  try{
    const guest=await context.newPage();await guest.goto(`http://127.0.0.1:${publisher.address().port}/`);
    const player=guest.frameLocator('iframe');
    await expect(player.locator('audio')).toHaveAttribute('src',`/music/embed/assets/${id}/audio?format=opus&bitrate=192`);
    await expect(player.locator('input[name="csrf"],form.logout')).toHaveCount(0);
    await playNativeAudio(player.locator('audio'),info);
    await expect.poll(()=>player.locator('audio').evaluate(audio=>audio.currentTime),{timeout:20000}).toBeGreaterThan(1);
    if(!noScript){const result=await new AxeBuilder({page:guest}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();expect(result.violations).toEqual([]);}
    expect(await guest.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
    await guest.screenshot({path:info.outputPath('music-external-opus-codec-player.png'),fullPage:true});
    const stream=`/music/embed/assets/${id}/audio?format=opus&bitrate=192`;
    await expect.poll(async()=>{
      const response=await context.request.head(stream);
      return response.status();
    },{timeout:20000}).toBe(200);
    expect((await context.request.get(stream,{headers:{Range:'bytes=0-31'}})).status()).toBe(206);
    await page.goto(`/music/assets/${id}`);await openDetailsFor(edit);await edit.getByLabel('Who can listen',{exact:true}).selectOption('private');await Promise.all([page.waitForNavigation({waitUntil:'domcontentloaded'}),edit.getByRole('button',{name:'Save track',exact:true}).press('Enter')]);
    expect((await context.request.get(stream,{headers:{Range:'bytes=0-31','If-None-Match':'*'}})).status()).toBe(404);
  }finally{await context.close();await new Promise((resolve,reject)=>publisher.close(error=>error?reject(error):resolve()));}
}
test('selected Opus widget decodes on a real external publisher',async({page,browser},info)=>journey(page,browser,info,false));
test('selected Opus iframe decodes without JavaScript @no-js',async({page,browser},info)=>journey(page,browser,info,true));
