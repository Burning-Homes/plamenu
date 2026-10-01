// Cold far seeks in genuine enhanced/no-JS decoders. An owned TCP bridge
// supplies a real shared 8-Mibit/s, 80-ms-header-RTT constrained profile.
// No media, frame, range, clock or browser buffering data is fabricated.
import { chromium } from 'playwright';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
const [manifestPath, controlUrl, evidence] = process.argv.slice(2);
const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
const browser = await chromium.launch();
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
const result = { status: 'running', lanes: [], budgets: { startupMs: 10000, farSeekMs: 10000,
  steadySeconds: 12, minimumPresentedFrames: 120, totalWaitMs: 2000, longestWaitMs: 2000,
  seekSeconds: 570, profile: { downstreamBytesPerSecond: 1048576, oneWayHeaderDelayMs: 40 } } };
function save() { writeFileSync(join(evidence, 'cold-browser.json'), JSON.stringify(result,null,2), { mode: 0o600 }); }
async function control(action) {
  const response = await fetch(`${controlUrl}/${action}`, { method: 'POST', headers: { 'X-AP09-Control': process.env.AP09_PROXY_TOKEN } });
  if (!response.ok) throw new Error(`owned network control failed ${response.status}`);
  return response.json();
}
async function snapshot(video) {
  return video.evaluate(v => ({ now: performance.now(), time: v.currentTime, seeking: v.seeking,
    paused: v.paused, ready: v.readyState, error: v.error?.code || null,
    presented: v.getVideoPlaybackQuality().totalVideoFrames-v.getVideoPlaybackQuality().droppedVideoFrames,
    dropped: v.getVideoPlaybackQuality().droppedVideoFrames, duration: v.duration,
    buffered: Array.from({length:v.buffered.length},(_,n) => [v.buffered.start(n),v.buffered.end(n)]) }));
}
async function until(video,predicate,milliseconds) {
  const began = Date.now();
  for (;;) {
    const sample = await snapshot(video);
    if (sample.error) throw new Error(`native decoder error ${sample.error}`);
    if (predicate(sample)) return sample;
    if (Date.now()-began > milliseconds) throw new Error(`decoder deadline exceeded: ${JSON.stringify(sample)}`);
    await pause(40);
  }
}
try {
  for (const constrained of [false,true]) {
    await control(constrained?'constrain':'unconstrain');
    for (const noScript of [false,true]) {
      const label = `${constrained?'8mbit-80ms':'local'}-${noScript?'no-js':'enhanced'}`;
      const context = await browser.newContext({ ignoreHTTPSErrors:true, javaScriptEnabled:!noScript,
        viewport:{width:320,height:800} });
      const page = await context.newPage();
      const errors=[],mediaRequests=[],sockets=[];
      page.on('pageerror',error => errors.push(String(error)));
      page.on('websocket',socket => sockets.push(new URL(socket.url()).pathname));
      page.on('request',request => { const url=new URL(request.url()); if(url.pathname.startsWith('/media/'))
        mediaRequests.push({path:url.pathname,range:request.headers().range||null,at:Date.now()}); });
      const beforeBytes = await control('status');
      await page.goto(manifest.serverOnlyEmbed,{waitUntil:'domcontentloaded'});
      const video=page.locator('video').first();
      await video.waitFor({state:'visible'});
      const requested=await video.evaluate(v => {v.muted=true;v.play().catch(()=>{});return performance.now();});
      const first=await until(video,s => !s.paused && s.time>.1 && s.presented>=3,15000);
      if(first.duration<599 || first.duration>602) throw new Error(`not the real ten-minute film: ${first.duration}`);
      const target=result.budgets.seekSeconds;
      if(first.buffered.some(([a,b]) => a<=target && b>=target)) throw new Error('far seek already buffered; this is not cold evidence');
      const seekRequestCount=mediaRequests.length;
      const sought=await video.evaluate((v,target) => {const now=performance.now();v.currentTime=target;v.play().catch(()=>{});return now;},target);
      const seekFrame=await until(video,s => !s.seeking && s.ready>=2 && s.time>=target+.15 && s.time<target+3 && s.presented>=first.presented+3,15000);
      const afterSeekRequests=mediaRequests.slice(seekRequestCount);
      if(!afterSeekRequests.length) throw new Error('far seek issued no actual new media request');
      const steadyBefore=await snapshot(video),samples=[];
      let waits=0,longest=0,activeWait=0,maxGap=0,previous=steadyBefore;
      while(previous.now-steadyBefore.now < result.budgets.steadySeconds*1000) {
        await pause(40);
        const current=await snapshot(video),gap=current.now-previous.now;
        maxGap=Math.max(maxGap,gap);
        if(previous.ready<3) {waits+=gap;activeWait+=gap;} else activeWait=0;
        longest=Math.max(longest,activeWait);samples.push(current);
        if(current.error) throw new Error('steady decoder error');
        previous=current;
      }
      const afterBytes=await control('status');
      const lane={label,applicationJavaScript:!noScript,startupMs:first.now-requested,farSeekMs:seekFrame.now-sought,
        beforeFarSeek:first,seekFrame,steadyBefore,steadyAfter:previous,steadySamples:samples,
        presentedSteadyFrames:previous.presented-steadyBefore.presented,waitMs:waits,longestWaitMs:longest,maxSampleGapMs:maxGap,
        measuredOriginMediaBodyBytes:afterBytes.origin_media_body_bytes-beforeBytes.origin_media_body_bytes,
        mediaRequests,afterSeekRequests,sockets,profile:afterBytes,errors};
      result.lanes.push(lane);save();
      if(lane.startupMs>result.budgets.startupMs || lane.farSeekMs>result.budgets.farSeekMs ||
        lane.presentedSteadyFrames<result.budgets.minimumPresentedFrames || lane.waitMs>result.budgets.totalWaitMs ||
        lane.longestWaitMs>result.budgets.longestWaitMs || maxGap>250 || errors.length || sockets.length)
        throw new Error(`cold decoder budget failed ${label}`);
      if(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth+1)) throw new Error(`320px overflow ${label}`);
      await page.screenshot({path:join(evidence,`cold-${label}-320px.png`)});
      await context.close();
    }
  }
  result.status='passed';save();
} catch(error) {result.status='failed';result.error=String(error);save();throw error;}
finally {await control('unconstrain').catch(()=>{});await browser.close();}
