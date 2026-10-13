import { test, expect } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import { execFileSync } from 'node:child_process';
import { readFileSync, readdirSync, mkdtempSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';

let directory, sprite, hlsFiles, longVideo;
test.beforeAll(() => {
  directory = mkdtempSync(join(tmpdir(), 'plamenu-controls-'));
  const extended=join(directory,'long.mp4');
  execFileSync('ffmpeg',['-v','error','-f','lavfi','-i','testsrc2=size=640x480:rate=10','-t','20','-an','-c:v','libx264','-threads','2','-preset','ultrafast','-pix_fmt','yuv420p','-movflags','+faststart',extended]);
  longVideo=readFileSync(extended);
  const file = join(directory, 'sprite.jpg');
  execFileSync('ffmpeg', ['-v','error','-f','lavfi','-i','testsrc2=size=480x90','-frames:v','1',file]);
  sprite = readFileSync(file);
  execFileSync('ffmpeg', ['-v','error','-i',process.env.A11Y_VIDEO_FIXTURE,
    '-filter_complex','[0:v]split=2[a][b];[a]scale=320:240[low];[b]scale=480:360[high]',
    '-map','[low]','-map','[high]','-c:v','libx264','-threads','2','-preset','ultrafast',
    '-b:v:0','300k','-b:v:1','600k','-pix_fmt','yuv420p','-g','30','-sc_threshold','0',
    '-force_key_frames','expr:gte(t,n_forced*1)','-f','hls','-hls_time','1',
    '-hls_playlist_type','vod','-var_stream_map','v:0,name:240 v:1,name:360',
    '-master_pl_name','master.m3u8','-hls_segment_filename',join(directory,'%v-%d.ts'),join(directory,'%v.m3u8')]);
  hlsFiles = new Map(readdirSync(directory).filter(name=>/\.(m3u8|ts)$/.test(name))
    .map(name=>[name,readFileSync(join(directory,name))]));
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
  if (options.hls) {
    requests.hls=0; requests.library=0;
    await page.route('**/controls-hls/**', async route => {
      requests.hls++;
      const name=new URL(route.request().url()).pathname.split('/').pop();
      await route.fulfill({status:hlsFiles.has(name)?200:404,
        contentType:name.endsWith('.m3u8')?'application/vnd.apple.mpegurl':'video/mp2t',
        body:hlsFiles.get(name) || ''});
    });
    await page.route('**/assets/hls.min.js?*', async route => {
      requests.library++;
      await route.fulfill({contentType:'application/javascript',
        body:readFileSync(new URL('../../crates/server/src/web/assets/hls.min.js',import.meta.url))});
    });
  }
  await page.route('**/controls-sprite.jpg', async route => {
    requests.sprite++;
    await route.fulfill({status:options.broken ? 500 : 200,contentType:'image/jpeg',body:sprite});
  });
  await page.route('**/controls-video.mp4', async route => {
    requests.media++;
    const bytes=options.long ? longVideo : readFileSync(process.env.A11Y_VIDEO_FIXTURE);
    const range=/^bytes=(\d+)-(\d*)$/.exec(route.request().headers().range || '');
    const start=range ? Number(range[1]) : 0;
    const end=range?.[2] ? Math.min(Number(range[2]),bytes.length-1) : bytes.length-1;
    await route.fulfill({status:range ? 206 : 200,contentType:'video/mp4',
      headers:{'Accept-Ranges':'bytes',...(range ? {'Content-Range':`bytes ${start}-${end}/${bytes.length}`} : {})},
      body:bytes.subarray(start,end+1)});
  });
  const board = {url:'/controls-sprite.jpg',columns:3,rows:1,count:3,interval:1,duration:3,labels};
  if (options.invalid) board.columns = 11;
  if (options.remote) { delete board.url; board.duration = null; }
  await page.route('**/controls-fixture', route => route.fulfill({contentType:'text/html',body:`<!doctype html>
    <html lang="en"><head><meta name="viewport" content="width=device-width,initial-scale=1"><title>VOD control regression fixture</title><link rel="stylesheet" href="/assets/app.css"></head>
    <body><main><h1>VOD controls</h1><div class="status__media" data-count="1"><figure class="media media--video">
    <video controls playsinline preload="none" src="/controls-video.mp4" ${options.live ? 'data-live="live"' : ''}
      ${options.hls ? 'data-hls="/controls-hls/master.m3u8" data-src="/controls-video.mp4"' : ''}
      ${options.standard ? 'data-src="/controls-video.mp4" data-original="/controls-original.mp4"' : ''}></video>
    ${options.standard ? '<p><a href="/controls-video.mp4">Open standard video (MP4, 720p)</a></p>' : ''}
    <details class="media__seek-previews" data-vod-controls='${JSON.stringify(board)}'><summary>Seek previews</summary>
      <a href="/controls-video.mp4#t=1">0:01</a></details></figure></div></main><script src="/assets/app.js"></script></body></html>`}));
  await page.addInitScript(() => {
    window.controlEvents=[];
    for (const kind of ['pointerdown','pointerup','pointercancel','lostpointercapture','input','change',
      'keydown','loadedmetadata','durationchange','emptied','timeupdate','seeking','seeked','play','playing','pause','ended','error']) {
      document.addEventListener(kind,event => {
        if (!(event.target instanceof HTMLMediaElement) && event.target.getAttribute?.('aria-label')!=='Seek') return;
        const video=document.querySelector('video');
        if (window.controlEvents.length>=80) return;
        window.controlEvents.push({kind,value:document.querySelector('[aria-label="Seek"]')?.value,
          time:video?.currentTime,ready:video?.readyState,seeking:video?.seeking,duration:video?.duration,
          seekable:video?.seekable.length,error:video?.error?.code,key:event.key,repeat:event.repeat,
          paused:video?.paused,ended:video?.ended,muted:video?.muted,controls:video?.controls,
          frames:video?.getVideoPlaybackQuality().totalVideoFrames});
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

test('VOD quality is visible before playback and fills the same row after real HLS discovery', async ({page}) => {
  const requests = await fixture(page,{hls:true});
  const player = page.locator('video');
  await page.locator('.vod-controls__menu > summary').click();
  const quality = page.getByRole('combobox',{name:'Video quality',exact:true,includeHidden:true});
  await expect(quality).toBeVisible();
  await expect(quality).toBeDisabled();
  await expect(quality.locator('option')).toHaveText(['Auto']);
  const original = await quality.elementHandle();
  expect(requests).toEqual({sprite:0,media:0,hls:0,library:0});
  await player.evaluate(video=>{video.loop=true;});
  const nativeOnly = await player.evaluate(video=>!window.MediaSource && !!video.canPlayType('application/vnd.apple.mpegurl'));
  await page.getByRole('button',{name:'Play',exact:true}).click();
  if (nativeOnly) {
    await expect.poll(()=>player.evaluate(video=>video.currentTime)).toBeGreaterThan(.1);
    await expect(quality).toBeDisabled();
    await expect(quality.locator('option')).toHaveText(['Auto']);
  } else {
    await expect(quality).toBeEnabled();
    await expect(quality.locator('option')).toHaveText(['Auto','240p','360p']);
    // Wait for the video's intrinsic dimensions and controls to settle
    // before reopening the menu; metadata can move it during HLS startup.
    await expect.poll(()=>player.evaluate(video=>video.currentTime),{timeout:15000}).toBeGreaterThan(.1);
    await page.locator('.vod-controls__menu > summary').click();
    await expect(quality).toBeVisible();
    await quality.selectOption('0');
    await expect.poll(()=>player.evaluate(video=>video.videoHeight),{timeout:15000}).toBe(240);
    // WebKit publishes resolution metadata before decoding. Qualify actual
    // playback before testing the immediate, buffered quality switch.
    await expect.poll(()=>player.evaluate(video=>video.readyState),{timeout:15000}).toBeGreaterThanOrEqual(2);
    await quality.selectOption('1');
    await expect.poll(()=>player.evaluate(video=>video.videoHeight),{timeout:15000}).toBe(360);
    expect(requests.library).toBe(1);
  }
  expect(requests.hls).toBeGreaterThan(0);
  expect(await quality.evaluate((node,previous)=>node===previous,original)).toBe(true);
  await expect(page.locator('.vod-controls__field').filter({has:quality})).toHaveCount(1);
  await player.evaluate(video=>video.pause());
});

test('VOD missing thumbnails preserve the player and pending quality', async ({page}) => {
  const requests = await fixture(page,{hls:true,broken:true});
  await page.getByRole('slider',{name:'Seek',exact:true}).focus();
  await expect.poll(() => requests.sprite).toBe(1);
  await expect(page.locator('.vod-controls')).toHaveCount(1);
  await expect(page.locator('.video-quality')).toHaveCount(1);
  expect(requests.media).toBe(0);expect(requests.hls).toBe(0);expect(requests.library).toBe(0);
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

test('VOD video surface routes keyboard and pointer playback once', async ({page}) => {
  const requests = await fixture(page);
  const video = page.locator('video');
  await expect(page.getByRole('button', {name:'Play',exact:true})).toBeVisible();
  expect(await video.evaluate(v => v.controls)).toBe(false);
  // Decode is verified by the first-party framed history journey. This case
  // isolates duplicate shortcut delivery and modifier/repeat handling.
  await video.evaluate(v => {
    let paused = true;
    window.playbackCalls = [];
    Object.defineProperty(v, 'paused', {get:()=>paused});
    v.play = () => {
      paused = false; window.playbackCalls.push('play'); v.dispatchEvent(new Event('play'));
      return Promise.resolve();
    };
    v.pause = () => { paused = true; window.playbackCalls.push('pause'); v.dispatchEvent(new Event('pause')); };
  });
  await video.dispatchEvent('keydown', {key:' ',ctrlKey:true});
  expect(await page.evaluate(() => window.playbackCalls)).toEqual([]);
  await video.focus();
  await video.press('Space');
  await expect(page.getByRole('button', {name:'Pause',exact:true})).toBeVisible();
  await video.dispatchEvent('keydown', {key:' ',repeat:true});
  expect(await page.evaluate(() => window.playbackCalls)).toEqual(['play']);
  await video.press('Space');
  await video.press('Enter');
  await video.click();
  await page.getByRole('button', {name:'Play',exact:true}).press('Space');
  await video.click();
  expect(await page.evaluate(() => window.playbackCalls)).toEqual(['play','pause','play','pause','play','pause']);
  expect(await video.evaluate(v => v.paused)).toBe(true);
  expect(requests.media).toBe(0);
});

test('VOD transient zero metadata preserves controls and pending seek', async ({page}) => {
  await fixture(page);
  const seek = page.getByRole('slider', {name:'Seek',exact:true});
  await expect(seek).toBeVisible();
  await seek.evaluate(input => {
    const video = document.querySelector('video');
    let ready = 0, duration = 0, position = 0;
    Object.defineProperty(video, 'readyState', {get:()=>ready});
    Object.defineProperty(video, 'duration', {get:()=>duration});
    Object.defineProperty(video, 'currentTime', {get:()=>position,set:value=>{position=value;}});
    video.load = ()=>{};
    input.value = '2';
    input.dispatchEvent(new Event('change'));
    ready = 4;
    video.dispatchEvent(new Event('loadedmetadata'));
    if (video.controls) throw new Error('Transient zero duration discarded enhanced controls');
    duration = 3;
    video.dispatchEvent(new Event('durationchange'));
  });
  await expect.poll(() => page.locator('video').evaluate(v => v.currentTime)).toBe(2);
  await expect(seek).toHaveValue('2');
  expect(await page.locator('video').evaluate(v => v.controls)).toBe(false);
});

test('VOD captions follow replaced tracks and playback state', async ({page}) => {
  await fixture(page);
  await expect(page.getByRole('slider',{name:'Seek',exact:true})).toBeVisible();
  const player = page.locator('video');
  await player.evaluate(video => { const track=video.addTextTrack('captions','English','en');track.mode='showing'; });
  await page.locator('.vod-controls__menu > summary').click();
  const captions = page.getByRole('combobox',{name:'Captions',exact:true});
  await expect(captions).toHaveValue('0');
  await captions.selectOption('-1');
  expect(await player.evaluate(video=>video.textTracks[0].mode)).toBe('disabled');
  await page.getByRole('button',{name:'Mute',exact:true}).click();
  expect(await player.evaluate(video=>video.muted)).toBe(true);
  await page.locator('.vod-controls__menu > summary').click();
  await page.getByRole('combobox',{name:'Playback speed'}).selectOption('1.5');
  expect(await player.evaluate(video=>video.playbackRate)).toBe(1.5);
  // Existing HLS code replaces external tracks; add/remove events must not
  // keep a stale caption selection or discard the replacement's mode.
  await player.evaluate(video => {const track=document.createElement('track');track.kind='captions';track.label='French';
    track.srclang='fr';video.append(track);track.track.mode='showing';track.remove();});
  await expect(captions.locator('option')).toHaveCount(2);
});

test('VOD compact controls keep mobile settings reachable with keyboard and touch', async ({page}) => {
  await page.setViewportSize({width:320,height:800});
  await fixture(page,{standard:true});
  const controls = page.locator('.vod-controls');
  await expect(page.getByRole('button',{name:'Play',exact:true})).toBeVisible();
  expect(await controls.evaluate(node=>node.getBoundingClientRect().height)).toBeLessThan(90);
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth+1)).toBe(true);
  const summary = page.locator('.vod-controls__menu > summary');
  await summary.press('Enter');
  const settings = page.locator('.vod-controls__settings');
  await expect(page.getByRole('combobox',{name:'Playback speed'})).toBeVisible();
  await expect.poll(()=>settings.evaluate(node=>{const r=node.getBoundingClientRect();return r.left>=0 && r.right<=innerWidth && r.top>=0 && r.bottom<=innerHeight;})).toBe(true);
  const volume = page.getByRole('slider',{name:'Volume',exact:true});
  await volume.press('Home');
  for (let step=0;step<10;step++) await volume.press('ArrowRight');
  expect(await page.locator('video').evaluate(video=>video.volume)).toBe(.5);
  await volume.press('Escape');
  await expect(summary).toBeFocused();
  await expect(settings).toBeHidden();
  await summary.press('Enter');
  const audit = await new AxeBuilder({page}).include('.vod-controls').withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
  expect(audit.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  await page.getByRole('button',{name:'Mute',exact:true}).click();
  await expect(settings).toBeHidden();
});

test('VOD standard video link returns to the figure on native fallback', async ({page}) => {
  await fixture(page,{standard:true});
  const link = page.getByRole('link',{name:'Open standard video (MP4, 720p)',includeHidden:true});
  await expect(link).toBeHidden();
  await page.locator('.vod-controls__menu > summary').press('Enter');
  await expect(link).toBeVisible();
  await expect(link).toHaveAttribute('href','/controls-video.mp4');
  await page.locator('video').dispatchEvent('error');
  await expect(page.locator('.vod-controls')).toHaveCount(0);
  await expect(link).toBeVisible();
  expect(await link.evaluate(node=>node.parentElement.parentElement.tagName)).toBe('FIGURE');
  expect(await page.locator('video').evaluate(video=>video.controls)).toBe(true);
});

test('live streams use the same enhanced controls', async ({page}) => {
  await fixture(page,{live:true});
  expect(await page.locator('video').evaluate(video=>video.controls)).toBe(false);
  await expect(page.locator('.vod-player--live')).toBeVisible();
  await expect(page.getByRole('button',{name:'Fullscreen',exact:true})).toBeVisible();
  await expect(page.locator('.media__seek-previews')).toBeHidden();
});
for (const options of [{invalid:true},{broken:true},{remote:true}]) {
  test(`VOD controls survive ${Object.keys(options)[0]} storyboard`, async ({page},info) => {
    const requests = await fixture(page,options);
    const player=page.locator('video');
    const seek=page.getByRole('slider',{name:'Seek',exact:true});
    if (options.remote) await expect(seek).toBeDisabled();
    if (options.broken) { await seek.focus(); await expect.poll(()=>requests.sprite).toBe(1); }
    expect(await player.evaluate(video=>video.controls)).toBe(false);
    await page.getByRole('button',{name:'Play',exact:true}).click();
    await expect.poll(()=>player.evaluate(video=>video.currentTime)).toBeGreaterThan(0);
    await expect(seek).toBeEnabled();
    expect(await player.evaluate(video=>video.error?.code ?? null)).toBeNull();
    await player.evaluate(video=>video.pause());
    await page.screenshot({path:info.outputPath('player-'+Object.keys(options)[0]+'.png')});
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
  await page.locator('.vod-controls__menu > summary').click();
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


test('VOD fullscreen uses the real browser surface when available', async ({page},info) => {
  await fixture(page);
  await page.getByRole('button',{name:'Play',exact:true}).click();
  await expect.poll(()=>page.locator('video').evaluate(v=>v.currentTime)).toBeGreaterThan(.2);
  await page.locator('video').evaluate(v=>v.pause());
  const supported=await page.evaluate(()=>document.fullscreenEnabled && Boolean(document.querySelector('.vod-player').requestFullscreen));
  if (supported) {
    const fullscreen=page.getByRole('button',{name:'Fullscreen',exact:true});
    await fullscreen.click();
    await expect.poll(()=>page.evaluate(()=>document.fullscreenElement?.className)).toBe('vod-player');
    await expect(fullscreen).toHaveAttribute('aria-pressed','true');
    const size = await page.locator('video').evaluate(v => ({height:v.getBoundingClientRect().height, viewport:innerHeight, max:getComputedStyle(v).maxHeight}));
    expect(size.max).toBe('none');
    expect(size.height).toBeGreaterThan(size.viewport - 160);
    await page.screenshot({path:info.outputPath('fullscreen.png')});
    await fullscreen.click();
    await expect.poll(()=>page.evaluate(()=>document.fullscreenElement)).toBeNull();
    await expect(fullscreen).toHaveAttribute('aria-pressed','false');
  } else {
    expect(await page.locator('video').evaluate(video=>video.controls)).toBe(false);
    await expect(page.getByRole('slider',{name:'Seek',exact:true})).toBeVisible();
  }
});


test('VOD preview stays within a narrow player after resize', async ({page}) => {
  await page.setViewportSize({width:1440,height:900});
  const requests=await fixture(page);
  const seek=page.getByRole('slider',{name:'Seek',exact:true});
  await seek.focus();
  // Preview intent only: do not start a load whose metadata legitimately closes the preview.
  await seek.evaluate(input=>{input.value='2';input.dispatchEvent(new Event('input'));});
  await expect(page.locator('.vod-controls__preview')).toBeVisible();
  await page.setViewportSize({width:320,height:800});
  await expect(page.locator('.vod-controls__preview')).toBeHidden();
  expect(await page.evaluate(()=>document.documentElement.scrollWidth)).toBeLessThanOrEqual(320);
  await seek.dispatchEvent('input');
  await expect(page.locator('.vod-controls__preview')).toBeVisible();
  expect(requests.media).toBe(0);
  const bounds=await page.locator('.vod-controls__preview').boundingBox();
  expect(bounds.x).toBeGreaterThanOrEqual(0);
  expect(bounds.x+bounds.width).toBeLessThanOrEqual(320);
});


test('VOD fullscreen fits a landscape phone', async ({page},info) => {
  await page.setViewportSize({width:844,height:390});
  await fixture(page);
  const supported=await page.evaluate(()=>document.fullscreenEnabled && Boolean(document.querySelector('.vod-player').requestFullscreen));
  test.skip(!supported,'This browser runtime does not expose DOM fullscreen.');
  await page.getByRole('button',{name:'Play',exact:true}).click();
  await expect.poll(()=>page.locator('video').evaluate(v=>v.currentTime)).toBeGreaterThan(.2);
  await page.locator('video').evaluate(v=>v.pause());
  await page.getByRole('button',{name:'Fullscreen',exact:true}).click();
  await expect.poll(()=>page.evaluate(()=>document.fullscreenElement?.className)).toBe('vod-player');
  const bounds=await page.locator('video').boundingBox();
  // Firefox fullscreen uses the virtual display, not the emulated viewport.
  const viewport=await page.evaluate(()=>({width:innerWidth,height:innerHeight}));
  expect(bounds.width,JSON.stringify({bounds,viewport})).toBe(viewport.width);
  expect(bounds.height).toBeGreaterThan(viewport.height-160);
  expect(await page.locator('.vod-controls').evaluate(e=>e.getBoundingClientRect().bottom)).toBeLessThanOrEqual(viewport.height);
  await page.screenshot({path:info.outputPath('fullscreen-landscape.png')});
  await page.getByRole('button',{name:'Fullscreen',exact:true}).click();
  await expect.poll(()=>page.evaluate(()=>document.fullscreenElement)).toBeNull();
});

test('fullscreen yields the whole picture and reveals controls on interaction', async ({page},info) => {
  await fixture(page,{long:true});
  const video=page.locator('video');
  const supported=await page.evaluate(()=>document.fullscreenEnabled && Boolean(document.querySelector('.vod-player').requestFullscreen));
  test.skip(!supported,'This browser runtime does not expose DOM fullscreen.');
  await video.evaluate(v=>{v.muted=true;v.load();});
  await expect.poll(()=>video.evaluate(v=>v.duration)).toBeGreaterThan(15);
  await page.getByRole('button',{name:'Play',exact:true}).click();
  await expect.poll(()=>video.evaluate(v=>v.currentTime)).toBeGreaterThan(.2);
  await page.getByRole('button',{name:'Fullscreen',exact:true}).click();
  await expect.poll(()=>page.evaluate(()=>Boolean(document.fullscreenElement))).toBe(true);
  const geometry=await video.evaluate(v=>({height:v.getBoundingClientRect().height,width:v.getBoundingClientRect().width,viewportWidth:innerWidth,viewportHeight:innerHeight}));
  expect(Math.abs(geometry.height-geometry.viewportHeight)).toBeLessThan(2);
  expect(Math.abs(geometry.width-geometry.viewportWidth)).toBeLessThan(2);
  await page.mouse.move(50,50);
  await expect.poll(()=>video.evaluate(v=>({opacity:getComputedStyle(v.closest('.vod-player').querySelector('.vod-controls')).opacity,paused:v.paused,duration:v.duration,focus:document.activeElement?.getAttribute('aria-label'),keyboardFocus:Boolean(v.closest('.vod-player').querySelector('.vod-controls :focus-visible')),settings:Boolean(v.closest('.vod-player').querySelector('details[open]'))})),{timeout:7000}).toMatchObject({opacity:'0',paused:false});
  await page.screenshot({path:info.outputPath('fullscreen-unobstructed.png')});
  if(info.project.use.hasTouch){
    await page.touchscreen.tap(100,100);
    expect(await video.evaluate(v=>v.paused)).toBe(false);
  }else await page.mouse.move(90,90);
  await expect.poll(()=>page.locator('.vod-controls').evaluate(e=>getComputedStyle(e).opacity)).toBe('1');
  await page.getByRole('slider',{name:'Seek',exact:true}).focus();
  await page.keyboard.press('ArrowRight');
  await expect(page.locator('.vod-player')).not.toHaveClass(/vod-player--idle/);
  await page.getByRole('button',{name:'Pause',exact:true}).click();
  await expect(page.locator('.vod-player')).not.toHaveClass(/vod-player--idle/);
  await page.evaluate(()=>document.exitFullscreen());
});
