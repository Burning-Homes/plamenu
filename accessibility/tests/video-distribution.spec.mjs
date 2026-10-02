import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {randomUUID} from 'node:crypto';
import {createServer} from 'node:http';
import {signIn,openVideoOptions,createVideoChannel,videoChannelForm} from '../session.mjs';
import {openVideoSettings,closeVideoSettings} from '../video-settings.mjs';

async function check(page,noScript){
  await page.setViewportSize({width:320,height:800});
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
  if(!noScript){
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
    expect(result.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
}
async function submit(page,button,path){
  const [response]=await Promise.all([page.waitForResponse(r=>new URL(r.url()).pathname===path&&r.request().method()==='POST'),button.press('Enter')]);
  expect(response.status()).toBe(303);
}
async function decoded(video){
  await video.evaluate(v=>{v.muted=true;v.play().catch(()=>{});});
  await expect.poll(()=>video.evaluate(v=>v.currentTime),{timeout:30000}).toBeGreaterThan(.2);
  expect(await video.evaluate(v=>v.videoWidth)).toBeGreaterThan(0);
  await video.evaluate(v=>v.pause());
}
async function journey(page,info,noScript){
  test.setTimeout(240000);await signIn(page);await page.goto('/videos/upload');await openVideoOptions(page);
  const form=await videoChannelForm(page);
  if(!await form.isVisible())await form.locator('..').locator(':scope > summary').press('Enter');
  const name=`sharing_${randomUUID().replaceAll('-','').slice(0,18)}`;
  await form.getByLabel('Channel address').fill(name);
  await form.getByLabel('Channel name').fill(name);
  await submit(page,form.getByRole('button',{name:'Create a video channel',exact:true}),'/web/video-channels');
  await page.goto('/videos/upload');await openVideoOptions(page);
  const upload=page.locator('form[action="/web/videos"]');
  await upload.getByRole('combobox',{name:'Channel',exact:true}).selectOption({label:name});
  await upload.getByLabel('Video title',{exact:true}).fill(`Sharing ${name}`);
  await upload.getByLabel('Visual description',{exact:true}).fill('Moving test pattern');
  await upload.getByLabel('Transcript',{exact:true}).fill('Steady tone');
  await upload.getByLabel('Video or audio file',{exact:true}).setInputFiles(process.env.A11Y_VIDEO_FIXTURE);
  await upload.getByRole('button',{name:'Upload video',exact:true}).press('Enter');
  await page.waitForURL(url=>/\/videos\/drafts\/\d+$/.test(url.pathname));
  const id=new URL(page.url()).pathname.split('/').pop();
  if(noScript){
    for(let i=0;i<60;i++){
      if(await page.getByRole('link',{name:'Review your video',exact:true}).isVisible())break;
      await page.waitForTimeout(1000);await page.getByRole('link',{name:'Refresh status',exact:true}).press('Enter');
    }
  }
  await expect(page.getByRole('link',{name:'Review your video',exact:true})).toBeVisible({timeout:60000});
  await page.getByRole('link',{name:'Review your video',exact:true}).press('Enter');
  await decoded(page.getByLabel('Video preview',{exact:true}));
  await page.getByRole('link',{name:'Downloads and embeds',exact:true}).press('Enter');
  const download=page.getByRole('checkbox',{name:'Offer a playable video download',exact:true});
  const embed=page.getByRole('checkbox',{name:'Allow this video in first-party embeds',exact:true});
  await expect(download).toBeChecked();await expect(embed).toBeChecked();
  await download.press('Space');await embed.press('Space');
  await submit(page,page.getByRole('button',{name:'Save sharing choices',exact:true}),`/web/videos/${id}/sharing`);
  await page.goto(`/videos/${id}/sharing?saved=1`);
  await expect(download).not.toBeChecked();await expect(embed).not.toBeChecked();
  for(const choice of await page.locator('.video-sharing .settings-field__choice').all()){
    // Firefox's fractional device-coordinate conversion can report 44 CSS
    // pixels as 43.999969; retain the target, round only floating-point noise.
    const box=await choice.boundingBox();expect(Math.round(box.height*100)/100).toBeGreaterThanOrEqual(44);
  }
  await check(page,noScript);await page.screenshot({path:info.outputPath('sharing-owner-320px.png'),fullPage:true});
  await page.getByRole('link',{name:'Return to your video',exact:true}).press('Enter');
  await page.getByRole('button',{name:'Publish video',exact:true}).press('Enter');
  await page.waitForURL(url=>/\/@[^/]+\/\d+/.test(url.pathname));
  const permalink=page.url(),status=new URL(permalink).pathname.split('/').pop();
  await decoded(page.locator('video').first());
  await expect(page.getByRole('link',{name:'Download playable video (MP4)',exact:true})).toHaveCount(0);
  expect((await page.request.get(`/web/statuses/${status}/embed?p2p=0`)).status()).toBe(404);
  await page.goto(`/videos/${id}/sharing`);await download.press('Space');await embed.press('Space');
  await submit(page,page.getByRole('button',{name:'Save sharing choices',exact:true}),`/web/videos/${id}/sharing`);
  await page.goto(permalink);
  await expect(page.locator('.media__settings')).toBeHidden();
  const settings=await openVideoSettings(page,noScript);
  const link=page.getByRole('link',{name:'Download playable video (MP4)',exact:true});
  const response=await page.request.get(await link.getAttribute('href'));
  expect(response.status()).toBe(200);expect(response.headers()['content-disposition']).toContain('attachment;');
  expect(response.headers()['cache-control']).toBe('private, no-store');
  const ranged=await page.request.get(await link.getAttribute('href'),{headers:{Range:'bytes=0-31'}});
  expect(ranged.status()).toBe(206);expect((await ranged.body()).length).toBe(32);
  const code=settings.getByRole('textbox',{name:'Embed this video',exact:true});
  await expect(code).toHaveCount(1);
  expect(await code.inputValue()).toContain('?p2p=1');
  if(!noScript){
    const sharing=settings.getByRole('checkbox',{name:'Share video with other viewers',exact:true});
    await expect(sharing).toBeVisible();
    await sharing.uncheck();
    expect(await code.inputValue()).toContain('?p2p=0');
    await page.keyboard.press('Escape');
    await expect(page.locator('.video-settings-dialog')).toHaveCount(0);
    await expect(page.locator('[data-status-menu] > summary').last()).toBeFocused();
    await openVideoSettings(page);
    await expect(sharing).not.toBeChecked();
    await expect(code).toHaveCount(1);
    // Native modal focus must stay inside it, including backwards traversal.
    await settings.getByRole('button',{name:'Close video settings',exact:true}).focus();
    await page.keyboard.press('Shift+Tab');
    expect(await page.evaluate(()=>!!document.activeElement.closest('.video-settings-dialog'))).toBe(true);
  }
  await code.focus();
  await code.press('ControlOrMeta+A');
  if(noScript){await expect(page.locator('[data-video-embed-copy]:visible')).toHaveCount(0);}
  else{
    await page.evaluate(()=>Object.defineProperty(navigator,'clipboard',{configurable:true,value:{writeText:()=>Promise.reject(new Error('fixture denied'))}}));
    const panel=code.locator('..').locator('..');
    await panel.getByRole('button',{name:'Copy embed code',exact:true}).press('Enter');
    await expect(panel.getByRole('status')).toHaveText('Select and copy the embed code manually');
    await expect(code).toBeFocused();
    await page.evaluate(()=>Object.defineProperty(navigator,'clipboard',{configurable:true,value:{writeText:value=>{window.copiedEmbed=value;return Promise.resolve();}}}));
    await panel.getByRole('button',{name:'Copy embed code',exact:true}).press('Enter');
    await expect(panel.getByRole('status')).toHaveText('Embed code copied');
    expect(await page.evaluate(()=>window.copiedEmbed)).toBe(await code.inputValue());
  }
  await check(page,noScript);await page.screenshot({path:info.outputPath('sharing-viewer-320px.png'),fullPage:true});
  const embedCode=await code.inputValue();
  if(!noScript)await closeVideoSettings(page);
  // A genuine separate local origin avoids both inheriting Plamenu's
  // frame-src restriction and a synthetic public origin's local-network gate.
  // No application CSP or browser protection is relaxed.
  const host=createServer((request,response)=>{
    response.setHeader('Content-Type','text/html');
    response.end(`<!doctype html><html lang="en"><head><title>Native embed host</title></head><body><main><h1>Embedded video</h1>${embedCode}</main></body></html>`);
  });
  await new Promise((resolve,reject)=>{host.once('error',reject);host.listen(0,'127.0.0.1',resolve);});
  try{
    await page.goto(`http://127.0.0.1:${host.address().port}/`);
    await expect(page.locator('iframe')).toHaveAttribute('title','Embedded post');
    await decoded(page.frameLocator('iframe').locator('video').first());
    expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
    await page.screenshot({path:info.outputPath('copied-iframe-decoded-320px.png'),fullPage:true});
  }finally{
    host.closeAllConnections();await new Promise(resolve=>host.close(resolve));
  }
  await page.goto(`/web/statuses/${status}/embed?p2p=0`);await decoded(page.locator('video').first());
  if(!noScript){
    await page.emulateMedia({reducedMotion:'reduce',forcedColors:'active'});await check(page,false);
  }
  await page.screenshot({path:info.outputPath('server-only-embed-320px.png'),fullPage:true});
  await page.goto(`/videos/${id}/sharing`);await embed.press('Space');
  await submit(page,page.getByRole('button',{name:'Save sharing choices',exact:true}),`/web/videos/${id}/sharing`);
  expect((await page.request.get(`/web/statuses/${status}/embed`)).status()).toBe(404);
  expect((await page.request.get(`/media/${id}/download`)).status()).toBe(200);
}
test('sharing choices preserve decoded playback, download ranges and accessible embed fallback',async({page},info)=>journey(page,info,false));
test('sharing choices preserve decoded playback, download ranges and accessible embed fallback @no-js',async({page},info)=>journey(page,info,true));
