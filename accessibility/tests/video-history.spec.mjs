import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {randomUUID} from 'node:crypto';
import {signIn} from '../session.mjs';

// This command runs against the harness's disposable developer account. A
// failed project must not leave preferences enabled for the next browser.
test.afterEach(async({page})=>{
  if(page.isClosed())return;
  await page.goto('/videos/settings');
  await page.getByRole('checkbox',{name:'Save my watch history',exact:true}).uncheck();
  await page.getByRole('checkbox',{name:'Share video with other viewers',exact:true}).check();
  await Promise.all([
    page.waitForResponse(r=>r.url().endsWith('/web/videos/settings')&&r.request().method()==='POST'&&r.status()===303),
    page.getByRole('button',{name:'Save video viewing settings',exact:true}).press('Enter'),
  ]);
});

async function check(page,noScript){
  await page.setViewportSize({width:320,height:800});
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth+1)).toBe(true);
  if (!noScript) {
    const results=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
    expect(results.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
}

async function decoded(video){
  try{
    await expect.poll(()=>video.evaluate(v=>v.currentTime)).toBeGreaterThan(.25);
    await expect.poll(()=>video.evaluate(v=>v.getVideoPlaybackQuality().totalVideoFrames)).toBeGreaterThan(0);
  }catch(error){
    const playback=await video.evaluate(v=>({time:v.currentTime,paused:v.paused,
      duration:v.duration,ready:v.readyState,network:v.networkState,source:v.currentSrc,error:v.error?.message,
      width:v.videoWidth,frames:v.getVideoPlaybackQuality().totalVideoFrames,
      buffered:[...Array(v.buffered.length)].map((_,i)=>[v.buffered.start(i),v.buffered.end(i)])}));
    throw new Error(`${error.message}; playback=${JSON.stringify(playback)}`);
  }
}

async function journey(page,noScript){
  test.setTimeout(180000);
  await signIn(page);
  await page.goto('/videos/settings');
  const enabled=page.getByRole('checkbox',{name:'Save my watch history',exact:true});
  await expect(enabled).not.toBeChecked();
  if(!noScript && await page.locator('body').getAttribute('data-p2p-configured')==='false'){
    await page.evaluate(()=>localStorage.setItem('plamenu:p2p-enabled','false'));
    await page.reload();
    await expect(page.getByRole('checkbox',{name:'Share video with other viewers',exact:true})).not.toBeChecked();
    await page.evaluate(()=>localStorage.removeItem('plamenu:p2p-enabled'));
    await page.reload();
    await expect(page.getByRole('checkbox',{name:'Share video with other viewers',exact:true})).toBeChecked();
  }
  await enabled.focus();
  await enabled.press('Space');
  await page.getByRole('checkbox',{name:'Share video with other viewers',exact:true}).uncheck();
  await page.getByRole('button',{name:'Save video viewing settings',exact:true}).press('Enter');
  await expect(enabled).toBeChecked();
  await expect(page.getByRole('checkbox',{name:'Share video with other viewers',exact:true})).not.toBeChecked();
  expect((await enabled.locator('..').boundingBox()).height).toBeGreaterThanOrEqual(44);
  await check(page,noScript);
  await page.getByRole('link',{name:'Watch history',exact:true}).click();
  await expect(page.getByText('No saved videos on this page.',{exact:true})).toBeVisible();
  await page.goto('/videos');
  const channel=page.locator('form[action="/web/video-channels"]');
  if(!await channel.isVisible()) await channel.locator('..').locator(':scope > summary').click();
  const name=`hist_${randomUUID().replaceAll('-','').slice(0,20)}`;
  await channel.getByLabel('Channel address').fill(name);
  await channel.getByLabel('Channel name').fill(`Watch ${name}`);
  await channel.getByRole('button',{name:'Create a video channel',exact:true}).press('Enter');
  await page.waitForURL(url=>url.pathname==='/videos');
  const upload=page.locator('form[action="/web/videos"]');
  const title=`Watch history film ${name}`;
  await upload.getByRole('combobox',{name:'Channel',exact:true}).selectOption({label:`Watch ${name}`});
  await upload.getByLabel('Video title').fill(title);
  await upload.getByLabel('Description',{exact:true}).fill('A moving test pattern and tone.');
  await upload.getByLabel('Visual description').fill('A moving colour test pattern.');
  await upload.getByLabel('Transcript',{exact:true}).fill('A steady tone.');
  await upload.getByLabel('Video or audio file',{exact:true}).setInputFiles(process.env.A11Y_VIDEO_FIXTURE);
  await upload.getByRole('button',{name:'Upload video',exact:true}).press('Enter');
  await page.waitForURL(url=>/\/videos\/drafts\/\d+$/.test(url.pathname));
  if(noScript){
    for(let attempt=0;attempt<60;attempt++){
      if(await page.getByRole('link',{name:'Review your video',exact:true}).isVisible())break;
      await page.getByRole('link',{name:'Refresh status',exact:true}).click();
      await page.waitForTimeout(500);
    }
  } else await expect(page.getByRole('link',{name:'Review your video',exact:true})).toBeVisible({timeout:45000});
  await page.getByRole('link',{name:'Review your video',exact:true}).click();
  await page.locator('form[action="/web/videos"]').getByRole('button',{name:'Publish video',exact:true}).press('Enter');
  await page.waitForURL(url=>/\/@[^/]+\/\d+/.test(url.pathname));
  const permalink=page.url();
  const statusId=new URL(permalink).pathname.split('/').pop();
  const connections=[];
  page.on('websocket',socket=>connections.push(socket.url()));
  page.on('request',request=>{if(request.url().includes('p2p-media-loader'))connections.push(request.url());});
  if(!noScript)await expect(page.locator('.media__p2p-toggle')).not.toBeChecked();
  const initial=await page.request.get('/videos/history');
  expect(initial.headers()['cache-control']).toContain('no-store');
  expect(await initial.text()).not.toContain(title);
  await page.getByText('Saved playback position',{exact:true}).click();
  const form=page.locator('form[data-video-history]');
  await form.getByLabel('Playback position (whole seconds)').fill('1');
  if(!noScript){
    await page.route('**/web/videos/*/position',route=>route.fulfill({status:503,body:'Temporary history outage'}));
    await form.getByRole('button',{name:'Save playback position',exact:true}).press('Enter');
    await expect(form.locator('[data-history-status]')).toContainText('Could not save the position.');
    await page.unroute('**/web/videos/*/position');
  }
  await form.getByRole('button',{name:'Save playback position',exact:true}).press('Enter');
  if(!noScript) {
    await expect(form.locator('[data-history-status]')).toHaveText('Playback position saved.');
    await page.goto('/videos/history');
  } else await page.waitForURL(url=>url.pathname==='/videos/history');
  await expect(page.getByRole('link',{name:title,exact:true})).toBeVisible();
  const resume=page.getByRole('link',{name:'Resume video',exact:true});
  await expect(resume).toHaveAttribute('href',/#t=1$/);
  await check(page,noScript);
  await page.goto(permalink);
  await page.getByText('Saved playback position',{exact:true}).click();
  const resumeInline=page.getByRole('link',{name:'Resume video 0:01',exact:true});
  await resumeInline.focus();
  if(noScript){
    const nativeUrl=await resumeInline.getAttribute('href');
    await resumeInline.press('Enter');
    await page.waitForURL(nativeUrl);
    const native=page.locator('video');
    await native.evaluate(video=>{video.muted=true;return video.play();});
    await expect.poll(()=>native.evaluate(v=>v.currentTime)).toBeGreaterThan(1);
    await native.evaluate(v=>v.pause());
  }else{
    const video=page.locator('figure.media--video video').first();
    await video.evaluate(v=>{v.muted=true;});
    const p2p=page.locator('.media__p2p-toggle');
    if(await p2p.count())await expect(p2p).not.toBeChecked();
    await resumeInline.press('Enter');
    await expect.poll(()=>video.evaluate(v=>v.currentTime)).toBeGreaterThanOrEqual(.95);
    await expect(video).toBeFocused();
    await video.evaluate(async v=>{try{await v.play();}catch(e){if(e.name!=='AbortError')throw e;}});
    await expect.poll(()=>video.evaluate(v=>v.getVideoPlaybackQuality().totalVideoFrames)).toBeGreaterThan(0);
    await expect.poll(()=>video.evaluate(v=>v.currentTime)).toBeGreaterThan(1.5);
    await video.evaluate(v=>{v.currentTime=0;v.pause();});
    await expect.poll(async()=>{
      const response=await page.request.get('/videos/history');
      return (await response.text()).includes('Saved playback position 0 s');
    }).toBe(true);
    // A second explicit correction queues behind a slow save; automatic
    // playback events must not discard it or clobber a focused numeric input.
    await page.route('**/web/videos/*/position',async route=>{
      await new Promise(resolve=>setTimeout(resolve,700));
      await route.continue();
    });
    const position=form.getByLabel('Playback position (whole seconds)');
    await position.fill('1');
    await form.getByRole('button',{name:'Save playback position',exact:true}).press('Enter');
    await position.fill('0');
    await form.getByRole('button',{name:'Save playback position',exact:true}).press('Enter');
    await expect.poll(async()=>{
      const response=await page.request.get('/videos/history');
      return (await response.text()).includes('Saved playback position 1 s');
    }).toBe(true);
    await expect.poll(async()=>{
      const response=await page.request.get('/videos/history');
      return (await response.text()).includes('Saved playback position 0 s');
    }).toBe(true);
    await page.unroute('**/web/videos/*/position');
    await page.emulateMedia({reducedMotion:'reduce',forcedColors:'active'});
    await check(page,false);
  }
  expect(connections.filter(url=>/\/tracker\/socket|p2p-media-loader/.test(url))).toEqual([]);
  // A fresh browser storage partition, and a read-only embed, both inherit
  // account opt-out from the ordinary non-JS form. No identity/CSRF is framed.
  const fresh=await page.context().browser().newContext({ignoreHTTPSErrors:true,
    javaScriptEnabled:!noScript,viewport:{width:320,height:800},
    extraHTTPHeaders:process.env.A11Y_CANONICAL_ORIGIN?{Origin:process.env.A11Y_CANONICAL_ORIGIN}:undefined});
  await fresh.addCookies(await page.context().cookies());
  const clean=await fresh.newPage();
  await clean.bringToFront();
  const freshConnections=[];
  clean.on('websocket',socket=>freshConnections.push(socket.url()));
  clean.on('request',request=>{if(request.url().includes('p2p-media-loader'))freshConnections.push(request.url());});
  try{
    for(const url of [permalink,`${new URL(permalink).origin}/web/statuses/${statusId}/embed`]){
      await clean.goto(url);
      expect(await clean.locator('body').getAttribute('data-p2p-default')).toBe('false');
      if(!noScript)await expect(clean.locator('.media__p2p-toggle')).not.toBeChecked();
      const video=clean.locator('video').first();
      await video.scrollIntoViewIfNeeded();
      await video.evaluate(async v=>{v.muted=true;try{await v.play();}catch(e){if(e.name!=='AbortError')throw e;}});
      await decoded(video);
      await video.evaluate(v=>v.pause());
      if(url.includes('/embed')){
        expect(await clean.locator('[data-p2p-csrf]').count()).toBe(0);
        expect(await clean.locator('form[data-video-history]').count()).toBe(0);
      }
    }
    expect(freshConnections.filter(url=>/\/tracker\/socket|p2p-media-loader/.test(url))).toEqual([]);
    await fresh.clearCookies();
    const embedded=`${new URL(permalink).origin}/web/statuses/${statusId}/embed?p2p=0`;
    // A distinct loopback origin tests cross-origin framing without Chromium's
    // public-site-to-loopback network block hiding our real embed response.
    const host='https://127.0.0.1:9444/';
    if(page.context().browser().browserType().name()==='chromium'){
      await fresh.grantPermissions(['local-network-access'],{origin:host});
    }
    await clean.route(host,route=>route.fulfill({contentType:'text/html',body:
      `<!doctype html><html lang="en"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Video embed harness</title><main><h1>Server-only video</h1><iframe width="300" height="600" allow="autoplay" title="Server-only video" src="${embedded}"></iframe></main></html>`}));
    await clean.goto(host);
    const frame=clean.frameLocator('iframe');
    await expect(frame.locator('body')).toHaveAttribute('data-p2p-default','false');
    if(!noScript)await expect(frame.locator('.media__p2p-toggle')).toBeDisabled();
    const embeddedVideo=frame.locator('video').first();
    await embeddedVideo.scrollIntoViewIfNeeded();
    await embeddedVideo.evaluate(v=>{v.muted=true;});
    if(page.context().browser().browserType().name()==='webkit'){
      // WebKit exposes its native start control in the video centre, but a
      // focused <video> does not implement Chromium's Space shortcut.
      await embeddedVideo.click();
    }else{
      await embeddedVideo.focus();
      await embeddedVideo.press('Space');
    }
    await decoded(embeddedVideo);
    expect(freshConnections.filter(url=>/\/tracker\/socket|p2p-media-loader/.test(url))).toEqual([]);
  }finally{await fresh.close();}
  await page.goto('/videos/history');
  await page.getByRole('button',{name:'Forget this video',exact:true}).press('Enter');
  await expect(page.getByText('No saved videos on this page.',{exact:true})).toBeVisible();
  await page.goto('/videos/settings');
  await page.getByRole('checkbox',{name:'Save my watch history',exact:true}).uncheck();
  await page.getByRole('checkbox',{name:'Share video with other viewers',exact:true}).check();
  await page.getByRole('button',{name:'Save video viewing settings',exact:true}).press('Enter');
  await expect(page.getByRole('checkbox',{name:'Save my watch history',exact:true})).not.toBeChecked();
  await page.goto(permalink);
  await expect(page.locator('form[data-video-history]')).toHaveCount(0);
  if(!noScript){
    const toggle=page.locator('.media__p2p-toggle');
    await expect(toggle).toBeChecked();
    await page.route('**/web/videos/p2p',route=>route.fulfill({status:503,body:'Temporary preference outage'}));
    await toggle.uncheck();
    await expect(page.locator('.media__p2p-status')).toContainText('account preference could not be saved');
    await page.unroute('**/web/videos/p2p');
    await toggle.check();
    await toggle.uncheck();
    await expect(page.locator('.media__p2p-status')).toHaveText('Peer-sharing preference saved for your account.');
    await page.goto('/videos/settings');
    await expect(page.getByRole('checkbox',{name:'Share video with other viewers',exact:true})).not.toBeChecked();
    await page.getByRole('checkbox',{name:'Share video with other viewers',exact:true}).check();
    await page.getByRole('button',{name:'Save video viewing settings',exact:true}).press('Enter');
    // Anonymous embedding can force server-only even with the account default
    // enabled, without relying on third-party cookies or shared localStorage.
    await page.goto(`${new URL(permalink).origin}/web/statuses/${statusId}/embed?p2p=0`);
    expect(await page.locator('body').getAttribute('data-p2p-default')).toBe('false');
    await expect(page.locator('.media__p2p-toggle')).not.toBeChecked();
    await expect(page.locator('.media__p2p-toggle')).toBeDisabled();
    // A server-only VOD remains playable after an actual HLS HTTP failure.
    // The recovery is bounded and visible, not a black native-control tile.
    await page.goto(permalink);
    await page.locator('.media__p2p-toggle').uncheck();
    await expect(page.locator('.media__p2p-status')).toHaveText('Peer-sharing preference saved for your account.');
    await page.route('**/media/hls/**',route=>route.fulfill({status:503,body:'Temporary HLS outage'}));
    const failed=page.locator('figure.media--video video').first();
    await failed.evaluate(v=>{v.muted=true;v.play().catch(()=>{});});
    await expect(page.getByRole('link',{name:'Open standard video',exact:true})).toBeVisible({timeout:25000});
    await decoded(failed);
    await expect(failed).toHaveAttribute('data-p2p-fallback','true');
    await expect(page.locator('.media__p2p-toggle')).not.toBeChecked();
    await expect(page.locator('.media__p2p-toggle')).toBeDisabled();
    await check(page,false);
    await page.unroute('**/media/hls/**');
  }
}
test('private Video history and decoded resume',async({page})=>journey(page,false));
test('private Video history and native resume @no-js',async({page})=>journey(page,true));
