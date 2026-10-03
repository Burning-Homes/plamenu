import { test, expect } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import { execFileSync } from 'node:child_process';
import { readFileSync, mkdtempSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';

let directory, sprite;
test.beforeAll(() => {
  directory = mkdtempSync(join(tmpdir(), 'plamenu-controls-'));
  const file = join(directory, 'sprite.jpg');
  execFileSync('ffmpeg', ['-v','error','-f','lavfi','-i','testsrc2=size=480x90','-frames:v','1',file]);
  sprite = readFileSync(file);
});
test.afterAll(() => rmSync(directory, {recursive:true, force:true}));
test.afterEach(async ({page},info) => {
  if (info.status!==info.expectedStatus) await info.attach('control-events', {
    body:JSON.stringify(await page.evaluate(()=>window.controlEvents || []),null,2),contentType:'application/json'});
});
const labels = {play:'Play',pause:'Pause',seek:'Seek',mute:'Mute',unmute:'Unmute',volume:'Volume',
  captions:'Captions',off:'Off',fullscreen:'Fullscreen',settings:'Playback settings',speed:'Playback speed',pip:'Picture in picture'};
async function fixture(page, options = {}) {
  const requests = {sprite:0, media:0};
  await page.route('**/controls-sprite.jpg', async route => {
    requests.sprite++;
    await route.fulfill({status:options.broken ? 500 : 200,contentType:'image/jpeg',body:sprite});
  });
  await page.route('**/controls-video.mp4', async route => {
    requests.media++;
    const bytes=readFileSync(process.env.A11Y_VIDEO_FIXTURE);
    const range=/^bytes=(\d+)-(\d*)$/.exec(route.request().headers().range || '');
    const start=range ? Number(range[1]) : 0;
    const end=range?.[2] ? Math.min(Number(range[2]),bytes.length-1) : bytes.length-1;
    await route.fulfill({status:range ? 206 : 200,contentType:'video/mp4',
      headers:{'Accept-Ranges':'bytes',...(range ? {'Content-Range':`bytes ${start}-${end}/${bytes.length}`} : {})},
      body:bytes.subarray(start,end+1)});
  });
  const board = {url:'/controls-sprite.jpg',columns:3,rows:1,count:3,interval:1,duration:3,labels};
  if (options.invalid) board.columns = 11;
  await page.route('**/controls-fixture', route => route.fulfill({contentType:'text/html',body:`<!doctype html>
    <html lang="en"><head><meta name="viewport" content="width=device-width,initial-scale=1"><title>VOD control regression fixture</title><link rel="stylesheet" href="/assets/app.css"></head>
    <body><main><h1>VOD controls</h1><div class="status__media"><figure class="media media--video">
    <video controls playsinline preload="none" src="/controls-video.mp4" ${options.live ? 'data-live="live"' : ''}></video>
    <details class="media__seek-previews" data-vod-controls='${JSON.stringify(board)}'><summary>Seek previews</summary>
      <a href="/controls-video.mp4#t=1">0:01</a></details></figure></div></main><script src="/assets/app.js"></script></body></html>`}));
  await page.addInitScript(() => {
    window.controlEvents=[];
    for (const kind of ['pointerdown','pointerup','pointercancel','lostpointercapture','input','change',
      'keydown','loadedmetadata','durationchange','emptied','timeupdate','seeking','seeked']) {
      document.addEventListener(kind,event => {
        if (!(event.target instanceof HTMLMediaElement) && event.target.getAttribute?.('aria-label')!=='Seek') return;
        const video=document.querySelector('video');
        if (window.controlEvents.length>=80) return;
        window.controlEvents.push({kind,value:document.querySelector('[aria-label="Seek"]')?.value,
          time:video?.currentTime,ready:video?.readyState,seeking:video?.seeking,duration:video?.duration,
          seekable:video?.seekable.length,error:video?.error?.code});
      },true);
    }
  });
  await page.goto('/controls-fixture');
  return requests;
}

test('VOD preview intent loads one sprite without loading or seeking media', async ({page}) => {
  const requests = await fixture(page);
  const seek = page.getByRole('slider',{name:'Seek',exact:true});
  await expect(seek).toBeVisible();
  expect(requests).toEqual({sprite:0,media:0});
  const player = page.locator('video');
  expect(await player.evaluate(video => video.controls)).toBe(false);
  await seek.focus();
  await expect.poll(() => page.locator('.vod-controls__tile img').evaluate(image => image.naturalWidth)).toBe(480);
  await seek.press('Tab'); await seek.focus();
  expect(requests).toEqual({sprite:1,media:0});
  expect(await player.evaluate(video => video.currentTime)).toBe(0);
  await page.setViewportSize({width:320,height:800});
  await seek.focus();
  const preview = page.locator('.vod-controls__preview');
  expect(await preview.evaluate(node => {const r=node.getBoundingClientRect();return r.left>=0 && r.right<=innerWidth;})).toBe(true);
  expect(await page.evaluate(() => document.documentElement.scrollWidth<=innerWidth+1)).toBe(true);
  await page.emulateMedia({reducedMotion:'reduce'});
  await page.screenshot({path:test.info().outputPath('vod-preview-320.png'),fullPage:true});
  const audit = await new AxeBuilder({page}).include('.vod-controls').withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
  expect(audit.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  await seek.press('ArrowRight');
  await expect.poll(()=>player.evaluate(video=>video.currentTime)).toBeGreaterThan(0);
  expect(requests.media).toBeGreaterThan(0);
});

test('VOD pointer cancel restores position and pointer release commits once', async ({page}) => {
  const requests = await fixture(page);
  const seek = page.getByRole('slider',{name:'Seek',exact:true});
  await expect(seek).toBeVisible();
  await seek.evaluate(input => {
    // Instrument actual seeks without depending on an encoder's seek latency.
    const video = document.querySelector('video'); let time=0; window.controlSeeks=[];
    Object.defineProperty(video,'readyState',{get:()=>1});
    Object.defineProperty(video,'duration',{get:()=>3});
    Object.defineProperty(video,'currentTime',{get:()=>time,set:value=>{time=value;window.controlSeeks.push(value);}});
    input.setPointerCapture=()=>{}; input.hasPointerCapture=()=>false;
    input.dispatchEvent(new PointerEvent('pointerdown',{pointerId:1,button:0}));
    input.value='1.5'; input.dispatchEvent(new Event('input'));
    input.dispatchEvent(new PointerEvent('pointercancel',{pointerId:1}));
    input.dispatchEvent(new Event('change'));
  });
  expect(await page.evaluate(()=>window.controlSeeks)).toEqual([]);
  await expect(seek).toHaveValue('0');
  await seek.evaluate(input => {
    input.dispatchEvent(new PointerEvent('pointerdown',{pointerId:2,button:0}));
    input.value='2'; input.dispatchEvent(new Event('input'));
    input.dispatchEvent(new PointerEvent('pointerup',{pointerId:2,clientX:input.getBoundingClientRect().left+input.getBoundingClientRect().width*2/3}));
    input.dispatchEvent(new Event('change'));
  });
  expect(await page.evaluate(()=>window.controlSeeks)).toEqual([2]);
  expect(requests.media).toBe(0);
  await page.locator('video').evaluate(video=>video.closest('figure').remove());
  await expect(page.locator('.vod-player')).toHaveCount(0);
});

test('VOD captions follow replaced tracks and playback state', async ({page}) => {
  await fixture(page);
  await expect(page.getByRole('slider',{name:'Seek',exact:true})).toBeVisible();
  const player = page.locator('video');
  await player.evaluate(video => { const track=video.addTextTrack('captions','English','en');track.mode='showing'; });
  const captions = page.getByRole('combobox',{name:'Captions',exact:true});
  await expect(captions).toHaveValue('0');
  await captions.selectOption('-1');
  expect(await player.evaluate(video=>video.textTracks[0].mode)).toBe('disabled');
  await page.getByRole('button',{name:'Mute',exact:true}).click();
  expect(await player.evaluate(video=>video.muted)).toBe(true);
  await page.getByText('Playback settings',{exact:true}).click();
  await page.getByRole('combobox',{name:'Playback speed'}).selectOption('1.5');
  expect(await player.evaluate(video=>video.playbackRate)).toBe(1.5);
  // Existing HLS code replaces external tracks; add/remove events must not
  // keep a stale caption selection or discard the replacement's mode.
  await player.evaluate(video => {const track=document.createElement('track');track.kind='captions';track.label='French';
    track.srclang='fr';video.append(track);track.track.mode='showing';track.remove();});
  await expect(captions.locator('option')).toHaveCount(2);
});

for (const options of [{live:true},{invalid:true},{broken:true}]) {
  test(`VOD native fallback ${Object.keys(options)[0]}`, async ({page}) => {
    await fixture(page,options);
    if (options.broken) {
      await page.getByRole('slider',{name:'Seek',exact:true}).focus();
      await expect(page.locator('.vod-controls')).toHaveCount(0);
    }
    expect(await page.locator('video').evaluate(video=>video.controls)).toBe(true);
    await expect(page.locator('.media__seek-previews')).toBeVisible();
  });
}


test('VOD pre-metadata seek survives an engine source replacement', async ({page}) => {
  await fixture(page);
  const seek = page.getByRole('slider',{name:'Seek',exact:true});
  await expect(seek).toBeVisible();
  await seek.evaluate(input => {
    const video=document.querySelector('video');let time=0,ready=0;
    Object.defineProperty(video,'readyState',{get:()=>ready});
    Object.defineProperty(video,'duration',{get:()=>3});
    Object.defineProperty(video,'currentTime',{get:()=>time,set:value=>{time=value;}});
    video.load=()=>{};
    input.value='2';input.dispatchEvent(new Event('change'));
    video.dispatchEvent(new Event('emptied'));
    // Represents the existing engine's later fallback-resume listener.
    video.addEventListener('loadedmetadata',()=>{video.currentTime=0;},{once:true});
    ready=1;video.dispatchEvent(new Event('loadedmetadata'));
  });
  await expect.poll(()=>page.locator('video').evaluate(video=>video.currentTime)).toBe(2);
  await expect(seek).toHaveValue('2');
});


test('VOD real pointer interaction commits the prospective seek', async ({page},info) => {
  await fixture(page);
  const seek=page.getByRole('slider',{name:'Seek',exact:true});
  const player=page.locator('video');
  await expect(seek).toBeVisible();
  await player.evaluate(video=>video.load());
  await expect.poll(()=>player.evaluate(video=>video.readyState)).toBeGreaterThanOrEqual(1);
  const box=await seek.boundingBox();
  if (info.project.use.hasTouch) {
    await page.touchscreen.tap(box.x+box.width*.7,box.y+box.height/2);
  } else {
    await page.mouse.move(box.x+box.width*.2,box.y+box.height/2);
    await page.mouse.down();
    await page.mouse.move(box.x+box.width*.7,box.y+box.height/2,{steps:4});
    expect(await player.evaluate(video=>video.currentTime)).toBe(0);
    await page.mouse.up();
  }
  await expect.poll(()=>player.evaluate(video=>video.currentTime)).toBeGreaterThan(1.5);
  await expect(page.locator('.vod-controls__preview')).toBeHidden();
});


test('VOD fullscreen and picture-in-picture reflect actual platform state', async ({page}) => {
  await page.addInitScript(() => {
    let fullscreen=null,pip=null;
    Object.defineProperty(document,'fullscreenElement',{get:()=>fullscreen});
    Object.defineProperty(document,'pictureInPictureEnabled',{get:()=>true});
    Object.defineProperty(document,'pictureInPictureElement',{get:()=>pip});
    Element.prototype.requestFullscreen=async function(){fullscreen=this;document.dispatchEvent(new Event('fullscreenchange'));};
    document.exitFullscreen=async()=>{fullscreen=null;document.dispatchEvent(new Event('fullscreenchange'));};
    HTMLVideoElement.prototype.requestPictureInPicture=async function(){pip=this;this.dispatchEvent(new Event('enterpictureinpicture'));};
    document.exitPictureInPicture=async()=>{const video=pip;pip=null;video.dispatchEvent(new Event('leavepictureinpicture'));};
  });
  await fixture(page);
  const fullscreen=page.getByRole('button',{name:'Fullscreen',exact:true});
  await fullscreen.click();
  await expect(fullscreen).toHaveAttribute('aria-pressed','true');
  expect(await page.evaluate(()=>document.fullscreenElement?.className)).toBe('vod-player');
  await fullscreen.click();
  await expect(fullscreen).toHaveAttribute('aria-pressed','false');
  await page.getByText('Playback settings',{exact:true}).click();
  const pip=page.getByRole('button',{name:'Picture in picture',exact:true});
  await expect(pip).toBeDisabled();
  await page.locator('video').evaluate(video=>video.load());
  await expect(pip).toBeEnabled();
  await pip.click();
  await expect(pip).toHaveAttribute('aria-pressed','true');
  expect(await page.evaluate(()=>document.pictureInPictureElement===document.querySelector('video'))).toBe(true);
  await pip.click();
  await expect(pip).toHaveAttribute('aria-pressed','false');
});


test('VOD fullscreen uses the real browser surface when available', async ({page}) => {
  await fixture(page);
  const supported=await page.evaluate(()=>document.fullscreenEnabled && Boolean(document.querySelector('.vod-player').requestFullscreen));
  if (supported) {
    const fullscreen=page.getByRole('button',{name:'Fullscreen',exact:true});
    await fullscreen.click();
    await expect.poll(()=>page.evaluate(()=>document.fullscreenElement?.className)).toBe('vod-player');
    await expect(fullscreen).toHaveAttribute('aria-pressed','true');
    await fullscreen.click();
    await expect.poll(()=>page.evaluate(()=>document.fullscreenElement)).toBeNull();
    await expect(fullscreen).toHaveAttribute('aria-pressed','false');
  } else {
    expect(await page.locator('video').evaluate(video=>video.controls)).toBe(false);
    await expect(page.getByRole('slider',{name:'Seek',exact:true})).toBeVisible();
  }
});
