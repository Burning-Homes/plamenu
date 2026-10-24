// Actual native JS/no-JS decoders and unmodified PeerTube mirror failover.
import { chromium } from 'playwright';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
const [manifestPath,evidence]=process.argv.slice(2);
const publications=JSON.parse(readFileSync(manifestPath,'utf8'));
const browser=await chromium.launch();
const observations=[];
const pause=ms=>new Promise(resolve=>setTimeout(resolve,ms));
async function sample(video) {
  return video.evaluate(v=>({time:v.currentTime,ready:v.readyState,paused:v.paused,seeking:v.seeking,error:v.error?.code||null,height:v.videoHeight,
    frames:v.getVideoPlaybackQuality().totalVideoFrames,dropped:v.getVideoPlaybackQuality().droppedVideoFrames,
    adaptiveFallback:v.dataset.p2pFallback||null}));
}
async function until(video,predicate,timeout) {
  const started=Date.now();
  while(Date.now()-started<timeout) {
    const value=await sample(video);
    if(value.error) throw new Error(`decoder error ${JSON.stringify(value)}`);
    if(predicate(value)) return value;
    await pause(50);
  }
  throw new Error(`decoder timeout ${JSON.stringify(await sample(video))}`);
}
try {
  for(const [index,publication] of publications.entries()) {
    for(const kind of publication.skip_peer?['native-js','native-no-js']:['native-js','native-no-js','peertube']) {
      const peer=kind==='peertube';
      const context=await browser.newContext({ignoreHTTPSErrors:true,javaScriptEnabled:kind!=='native-no-js',viewport:{width:peer?800:320,height:800}});
      const page=await context.newPage();
      const errors=[];
      const mediaResponses=[],failedRequests=[],playerEvents=[];
      let blocked=0,mirrorResponses=0,originalInit=0;
      page.on('pageerror',error=>errors.push(String(error)));
      page.on('console',message=>{
        const text=message.text();
        const event=text.includes('Removed redundancy of segment URL')?'redundancy-removed':
          /Segment .* error\./.test(text)?'segment-error':null;
        if(event&&playerEvents.length<64)playerEvents.push({event,timeMs:Date.now()});
      });
      const publicUrl=value=>{const url=new URL(value);return `${url.origin}${url.pathname}`;};
      page.on('requestfailed',request=>failedRequests.push({url:publicUrl(request.url()),reason:request.failure()?.errorText}));
      page.on('response',response=>{
        if(/\.mp4$|\/media\/mirrors\//.test(new URL(response.url()).pathname))
          mediaResponses.push({url:publicUrl(response.url()),status:response.status(),range:response.request().headers().range||null});
      });
      try {
        if(peer) {
          await page.goto(new URL('/login',publication.peertube_url).href);
          await page.locator('#username').fill(process.env.PEERTUBE_E2E_USER);
          await page.locator('#password').fill(process.env.PEERTUBE_E2E_PASSWORD);
          await page.locator('input[type="submit"][value="Login"]').click();
          await page.waitForURL(url=>url.pathname!=='/login');
          const sourceHost=new URL(publication.peertube_url).host;
          await page.route('**/*.mp4*',async route=>{
            // Stock PeerTube applies redundancy substitutions to media
            // segments, not its hls.js initialization loader. Permit only a
            // finite small bootstrap range, never a full original transfer.
            const init=route.request().headers().range?.match(/^bytes=0-(\d+)$/);
            if(new URL(route.request().url()).host===sourceHost&&init&&Number(init[1])<256*1024){originalInit++;await route.continue();}
            else if(new URL(route.request().url()).host===sourceHost){
              blocked++;
              // A stalled original read then a real network failure. Stock
              // PeerTube keys mirror decisions by file URL, not byte range:
              // immediate parallel failures can discard a healthy mirror.
              // The hard-error counterexample is retained separately.
              await pause(500);
              await route.abort('failed').catch(()=>{});
            }
            else await route.continue();
          });
          page.on('response',response=>{
            if(new URL(response.url()).pathname.startsWith('/media/mirrors/')&&response.ok())mirrorResponses++;
          });
        }
        await page.goto(peer?publication.peertube_url:publication.native_url,{waitUntil:'domcontentloaded'});
        if(peer) {
          const consent=page.getByRole('button',{name:'OK',exact:true});
          await consent.waitFor({state:'visible',timeout:3000}).catch(()=>{});
          if(await consent.isVisible())await consent.click();
        }
        const video=page.locator(peer?'video.vjs-tech':'video[data-hls]').first();
        await video.waitFor({state:'attached'});
        if(peer&&await page.locator('.vjs-big-play-button').isVisible())await page.locator('.vjs-big-play-button').click();
        if(peer){
          // The complete copied ladder is checked before this browser phase.
          // Exercise failover/seek at one explicitly selected rung so Auto's
          // simultaneous rendition changes do not create a different outage
          // workload on each run. Use the stock receiving player's controls.
          await video.hover({force:true});
          await page.locator('.vjs-settings').click();
          await page.locator('.vjs-main-menu .vjs-menu-item').filter({hasText:'Quality'}).click();
          const menu=page.locator('.vjs-menu').filter({
            has:page.locator('.vjs-back-button').filter({hasText:'Quality'}),
          });
          await menu.locator('[role="menuitemradio"]',{hasText:'480p'}).click();
        }
        const started=Date.now();
        await video.evaluate(v=>{v.muted=true;v.play().catch(()=>{});});
        const initial=await until(video,s=>!s.paused&&s.ready>=2&&s.frames>=3&&(!peer||s.height===480),10000);
        if(peer&&await video.evaluate(v=>v.videoHeight)!==480)throw new Error('selected mirror rendition did not decode');
        const startupMs=Date.now()-started;
        const seeking=Date.now();
        await video.evaluate(v=>{v.currentTime=12;});
        const seek=await until(video,s=>s.time>=12&&s.time<15&&!s.seeking&&s.ready>=2&&s.frames>initial.frames+3,5000);
        const seekMs=Date.now()-seeking;
        await pause(2000);
        const steady=await until(video,s=>s.frames>=seek.frames+15&&!s.paused,5000);
        if(kind==='native-js'&&steady.adaptiveFallback)throw new Error('mirror adaptive playback silently fell back to progressive');
        const reflow=await page.evaluate(()=>({width:innerWidth,scroll:document.documentElement.scrollWidth}));
        if(errors.length||steady.error||reflow.scroll>reflow.width)throw new Error(`playback/reflow ${JSON.stringify({errors,steady,reflow})}`);
        if(peer&&!blocked){
          // Stock PeerTube randomly load-balances healthy mirrors, and can
          // choose the copy for every segment of this short movie. Prove the
          // original media outage with one real page request in that case;
          // do not override its RNG, loader, responses or receiving player.
          const bootstrap=mediaResponses.find(response=>new URL(response.url).host===new URL(publication.peertube_url).host&&/^bytes=0-\d+$/.test(response.range||''));
          if(!bootstrap)throw new Error('missing bounded original initialization evidence');
          const start=Number(bootstrap.range.split('-')[1])+1;
          const reachable=await page.evaluate(async({url,start})=>{
            try{await fetch(url,{headers:{Range:`bytes=${start}-${start+1}`},cache:'no-store'});return true;}catch{return false;}
          },{url:bootstrap.url,start});
          if(reachable)throw new Error('original media outage probe unexpectedly succeeded');
        }
        if(peer&&(!blocked||!mirrorResponses))throw new Error(`missing real PeerTube mirror decode under original-media outage ${JSON.stringify({blocked,mirrorResponses})}`);
        await page.screenshot({path:join(evidence,`mirror-${index}-${kind}.png`)});
        observations.push({kind,initial,seek,steady,startupMs,seekMs,reflow,errors,blockedOriginalRequests:blocked,originalInitializationRanges:originalInit,actualMirrorResponses:mirrorResponses,mediaResponses,playerEvents});
      } catch(error) {
        observations.push({kind,failure:String(error),errors,mediaResponses,failedRequests,blockedOriginalRequests:blocked,originalInitializationRanges:originalInit,actualMirrorResponses:mirrorResponses,playerEvents});
        await page.screenshot({path:join(evidence,`mirror-failed-${index}-${kind}.png`)}).catch(()=>{});
        throw error;
      } finally {await context.close();}
    }
  }
} catch(error) {
  observations.push({failure:String(error)});
  throw error;
} finally {
  writeFileSync(join(evidence,'mirror-playback.json'),JSON.stringify(observations,null,2),{mode:0o600});
  await browser.close();
}
