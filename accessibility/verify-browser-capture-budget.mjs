// Synthetic camera/screen pixels and tone; real first-party capture controls,
// MediaRecorder, authenticated WebSocket, native encoding and three decoders.
import { chromium } from 'playwright';
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { observeCaptureWire } from './capture-wire-observer.mjs';
const [manifestPath,evidence]=process.argv.slice(2);
const manifest=JSON.parse(readFileSync(manifestPath,'utf8'));
const browser=await chromium.launch();
const result={status:'running',phases:[],failures:[],budgets:{onAirMs:30000,startupMs:20000,measuredSeconds:20,p95GlassMs:10000,
  // PeerTube 8.3 DEFAULT mode deliberately retains five 2s segments. This
  // foreign player is a decode/progress observation, not our native 10s gate.
  foreignPlayerLatency:'observed separately; stock PeerTube DEFAULT five-segment margin',
  minimumFramesPerSecond:10,minimumMediaSecondsPerSecond:.5},source:'synthetic 640x360/15fps camera/screen canvas and 440Hz Web Audio; not hardware capture'};
const pause=ms=>new Promise(resolve=>setTimeout(resolve,ms));
function save(stage){result.stage=stage;writeFileSync(join(evidence,'capture-browser.json'),JSON.stringify(result,null,2),{mode:0o600});}
async function sample(video){return video.evaluate(v=>{
  let latencyMs=null;
  if(v.readyState>=2){const c=document.createElement('canvas');c.width=320;c.height=180;
    const x=c.getContext('2d',{willReadFrequently:true});x.drawImage(v,0,0,320,180);
    const p=x.getImageData(0,6,320,1).data;let ticks=0;
    for(let bit=0;bit<32;bit++){const at=(bit*10+5)*4;if(p[at]+p[at+1]+p[at+2]>384)ticks+=2**bit;}
    const now=Math.floor(Date.now()/100)%(2**32),elapsed=(now-ticks+2**32)%(2**32);
    if(elapsed<=300)latencyMs=elapsed*100;
  }
  return{sampledAtMs: performance.now(), playbackRate: v.playbackRate, seeking: v.seeking,
    bufferedRanges: [...Array(v.buffered.length)].map((_, n) => [v.buffered.start(n), v.buffered.end(n)]),
    seekableRanges: [...Array(v.seekable.length)].map((_, n) => [v.seekable.start(n), v.seekable.end(n)]),
    visibilityClass: document.visibilityState === 'visible' ? 'visible' : 'hidden',
    sourceClass: !v.currentSrc ? 'empty' : v.currentSrc.startsWith('blob:') ? 'blob-media-source' : 'resource-source',
    profile: window.__ownedVideoProfiles?.get(v)?.() || null,
    time:v.currentTime,ready:v.readyState,paused:v.paused,error:v.error?.code||null,
    presented:v.getVideoPlaybackQuality().totalVideoFrames-v.getVideoPlaybackQuality().droppedVideoFrames,
    dropped:v.getVideoPlaybackQuality().droppedVideoFrames,latencyMs};
});}
async function decoded(video){const started=Date.now();for(;;){const state=await sample(video);
  if(state.error)throw new Error(`capture decoder error ${state.error}`);
  if(!state.paused&&state.presented>=8&&state.time>.2)return{...state,startupMs:Date.now()-started};
  if(Date.now()-started>20000)throw new Error('capture player did not decode within startup budget');await pause(100);}}
const creatorContext=await browser.newContext({ignoreHTTPSErrors:true,viewport:{width:320,height:800}});
await creatorContext.addInitScript(()=>{
  async function source(kind){
    const canvas=document.createElement('canvas');canvas.width=640;canvas.height=360;
    const context=canvas.getContext('2d');let frame=0;
    function draw(){context.fillStyle=kind==='screen'?'#11bb55':'#315fdb';context.fillRect(0,0,640,360);
      context.fillStyle='#ffbb22';context.fillRect((frame++*9)%600,80,40,120);
      const ticks=Math.floor(Date.now()/100)%(2**32);
      for(let bit=0;bit<32;bit++){context.fillStyle=Math.floor(ticks/2**bit)%2?'rgb(235,235,235)':'rgb(16,16,16)';context.fillRect(bit*20,0,20,24);}}
    draw();const timer=setInterval(draw,66);
    const stream=canvas.captureStream(15),audio=new AudioContext(),tone=audio.createOscillator(),destination=audio.createMediaStreamDestination();
    tone.frequency.value=440;tone.connect(destination);tone.start();
    const combined=new MediaStream([...stream.getVideoTracks(),...destination.stream.getAudioTracks()]);
    window.__ownedCaptureSource={kind,stream:combined,dispose:()=>{clearInterval(timer);tone.stop();audio.close();}};
    combined.getVideoTracks()[0].addEventListener('ended',()=>{clearInterval(timer);tone.stop();audio.close();});
    return combined;
  }
  Object.defineProperty(navigator.mediaDevices,'getUserMedia',{configurable:true,value:()=>source('camera')});
  Object.defineProperty(navigator.mediaDevices,'getDisplayMedia',{configurable:true,value:()=>source('screen')});
});
const creator=await creatorContext.newPage();
let peerAuth;
const contexts=[];
const wireObservers=[];
try{
  await creator.goto(new URL('/login',manifest.manage).href);
  await creator.locator('input[name="identifier"]').fill(process.env.AP09_CAPTURE_USER);
  await creator.locator('input[name="password"]').fill(process.env.AP09_CAPTURE_PASSWORD);
  await creator.locator('button[type="submit"]').click();
  await creator.waitForURL(url=>url.pathname==='/');
  await creator.goto(manifest.manage);
  for(const kind of ['camera','screen']){
    const publisher=creator.locator('[data-live-browser]');
    const started=Date.now();
    await publisher.getByRole('button',{name:kind==='camera'?'Use camera and microphone':'Share screen',exact:true}).press('Enter');
    for(;;){const response=await creator.request.get(new URL(`/api/v1/statuses/${manifest.status}`,manifest.manage).href);
      if(response.ok()&&(await response.json()).media_attachments[0].live?.state==='live')break;
      if(Date.now()-started>result.budgets.onAirMs)throw new Error(`${kind} never on air`);await pause(200);}
    const onAirMs=Date.now()-started;
    save(`${kind}-on-air`);
    const remoteFile=join(evidence,`remote-${kind}.json`),until=Date.now()+90000;
    while(!existsSync(remoteFile)){if(Date.now()>until)throw new Error('owned actual PeerTube import not supplied');await pause(100);}
    const remote=JSON.parse(readFileSync(remoteFile,'utf8'));
    const clients=[];
    for(const player of ['native-js','native-no-js','peertube']){
      const peer=player==='peertube';
      const context=await browser.newContext({ignoreHTTPSErrors:true,javaScriptEnabled:player!=='native-no-js',
        viewport:{width:peer?800:320,height:800},...(peer&&peerAuth?{storageState:peerAuth}:{})});
      if(player!=='native-no-js')await context.addInitScript(()=>{window.__ownedVideoProfile=true;});
      contexts.push(context);const page=await context.newPage(),errors=[];
      let stopWire = null;
      if(player==='native-no-js'&&process.env.AP09_CAPTURE_TRACE==='1'){
        try{
          stopWire=await observeCaptureWire(context,page,evidence,kind);
          wireObservers.push(stopWire);
        }catch{
          writeFileSync(join(evidence,`capture-${kind}-nojs-wire.json`),JSON.stringify({kind,unavailable:true}),{mode:0o600});
        }
      }
      page.on('pageerror',error=>errors.push(String(error)));
      if(peer&&!peerAuth){await page.goto(new URL('/login',remote.url).href);
        await page.locator('#username').fill(process.env.PEERTUBE_E2E_USER);await page.locator('#password').fill(process.env.PEERTUBE_E2E_PASSWORD);
        await page.locator('input[type="submit"][value="Login"]').click();await page.waitForURL(url=>url.pathname!=='/login');peerAuth=await context.storageState();}
      await page.goto(peer?remote.url:manifest.watch,{waitUntil:'domcontentloaded'});
      if(peer){const consent=page.getByRole('button',{name:'OK',exact:true});await consent.waitFor({state:'visible',timeout:3000}).catch(()=>{});
        if(await consent.isVisible())await consent.click();}
      const video=page.locator(peer?'video.vjs-tech':'video[data-hls]').first();await video.waitFor({state:'attached'});
      if(stopWire)await video.evaluate(v=>{
        window.__ownedCaptureEvents=[];
        for(const event of ['waiting','playing','stalled','seeking','seeked','error'])v.addEventListener(event,()=>{
          window.__ownedCaptureEvents.push({event,at:performance.now(),time:v.currentTime,ready:v.readyState,
            buffered:[...Array(v.buffered.length)].map((_,n)=>[v.buffered.start(n),v.buffered.end(n)])});
        });
      });
      if(peer&&await page.locator('.vjs-big-play-button').isVisible())await page.locator('.vjs-big-play-button').click();
      await video.evaluate(v=>{v.muted=true;v.play().catch(()=>{});});
      clients.push({player,page,video,context,errors,stopWire,start:await decoded(video)});
    }
    await pause(8000);const before=await Promise.all(clients.map(client=>sample(client.video))),observations=[];
    const untilMeasured=Date.now()+result.budgets.measuredSeconds*1000;
    while(Date.now()<untilMeasured){observations.push(await Promise.all(clients.map(client=>sample(client.video))));await pause(200);}
    const after=await Promise.all(clients.map(client=>sample(client.video)));
    const phase={kind,onAirMs,clients:[],observations};
    result.phases.push(phase);
    for(let n=0;n<clients.length;n++){
      const client=clients[n],latencies=observations.map(values=>values[n].latencyMs).filter(value=>value!==null).sort((a,b)=>a-b);
      const p95Ms=latencies[Math.floor(latencies.length*.95)];
      const measured={player:client.player,start:client.start,before:before[n],after:after[n],samples:latencies.length,p95Ms,maxMs:latencies.at(-1)};
      phase.clients.push(measured);
      if(client.stopWire){
        measured.playbackEvents=await client.page.evaluate(()=>window.__ownedCaptureEvents);
        await client.stopWire();wireObservers.splice(wireObservers.indexOf(client.stopWire),1);
      }
      if(latencies.length<result.budgets.measuredSeconds*2||(client.player!=='peertube'&&p95Ms>result.budgets.p95GlassMs)||after[n].error||client.errors.length||
        after[n].presented-before[n].presented<result.budgets.measuredSeconds*result.budgets.minimumFramesPerSecond||
        after[n].time-before[n].time<result.budgets.measuredSeconds*result.budgets.minimumMediaSecondsPerSecond)
        result.failures.push(`decoded ${kind} capture budget failed: ${JSON.stringify(measured)}`);
      if(await client.page.evaluate(()=>document.documentElement.scrollWidth>innerWidth+1))result.failures.push(`capture reflow failure ${kind} ${client.player}`);
      await client.page.screenshot({path:join(evidence,`capture-${kind}-${client.player}-320px.png`)});
      await client.context.close();
    }
    save(`${kind}-measured`);
    await publisher.getByRole('button',{name:'Stop broadcasting',exact:true}).click();
    await creator.evaluate(()=>{
      if(!window.__ownedCaptureSource.stream.getTracks().every(track=>track.readyState==='ended'))throw new Error('first-party stop did not release capture tracks');
      window.__ownedCaptureSource.dispose();
    });
    // Close sources through the actual creator control; phase two reuses the
    // stable permanent broadcast and obtains a new recording generation.
    await pause(2000);
  }
  if(result.failures.length)throw new Error(result.failures.join(' | '));
  result.status='passed';save('complete');
}catch(error){result.status='failed';result.error=String(error);save('failed');throw error;}
finally{for(const stopWire of wireObservers)await stopWire().catch(()=>{});for(const context of contexts)await context.close().catch(()=>{});await browser.close();}
