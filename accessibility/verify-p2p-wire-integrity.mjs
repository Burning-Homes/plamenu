// Both real origin products: malicious native seed, untouched native receiver,
// actual PeerTube exchange, explicit opt-out and genuine no-JS playback.
import {chromium} from 'playwright';
import {readFileSync,writeFileSync} from 'node:fs';
import {join} from 'node:path';
import {installPeerWireFault} from './p2p-wire-fault.mjs';
const [manifestPath,evidence]=process.argv.slice(2);
const manifest=JSON.parse(readFileSync(manifestPath,'utf8'));
const browser=await chromium.launch();
const contexts=[];
const result={status:'running',origin:manifest.origin,phases:[],budgets:{peerSeconds:45,faultSeconds:45,
  fallbackSeconds:20,steadySeconds:12,minimumPresentedFramesPerSecond:10},
  fault:'actual outbound RTC raw media chunk mutation/replay; framed commands and receiving product unchanged'};
const pause=ms=>new Promise(resolve=>setTimeout(resolve,ms));
const segments=/(?:\/media\/.*(?:\.m4s(?:\?|$)|\/seg\?)|\/[^/]+-fragmented\.mp4(?:\?|$))/;
const safe=url=>{const u=new URL(url);return `${u.origin}${u.pathname}`;};
function save(stage){result.stage=stage;writeFileSync(join(evidence,'peer-integrity-browser.json'),JSON.stringify(result,null,2),{mode:0o600});}
async function sample(video){return video.evaluate(v=>({time:v.currentTime,ready:v.readyState,error:v.error?.code||null,
  presented:v.getVideoPlaybackQuality().totalVideoFrames-v.getVideoPlaybackQuality().droppedVideoFrames,
  dropped:v.getVideoPlaybackQuality().droppedVideoFrames,validated:Number(v.dataset.p2pValidated||0),
  downloaded:Number(v.dataset.p2pDownloaded||0),uploaded:Number(v.dataset.p2pUploaded||0),
  validationErrors:Number(v.dataset.p2pValidationErrors||0)+Number(v.dataset.hlsHttpValidationErrors||0),
  peerValidationErrors:Number(v.dataset.p2pValidationErrors||0),httpValidationErrors:Number(v.dataset.hlsHttpValidationErrors||0),height:v.videoHeight}));}
async function until(predicate,seconds,message){const end=Date.now()+seconds*1000;
  while(Date.now()<end){if(await predicate())return;await pause(200);}throw new Error(message);}
async function open(kind,{fault=false,delay=0,off=false,noJS=false,deferDecode=false}={}){
  const context=await browser.newContext({ignoreHTTPSErrors:true,javaScriptEnabled:!noJS,
    viewport:{width:kind==='peertube'?800:320,height:800}});contexts.push(context);
  if(fault)await context.addInitScript(installPeerWireFault);
  const page=await context.newPage(),sockets=[],errors=[],responses=[];
  page.on('pageerror',error=>errors.push(String(error)));
  page.on('websocket',socket=>sockets.push(safe(socket.url())));
  page.on('response',response=>{if(response.status()<300&&response.url().includes('/media/'))
    responses.push({url:safe(response.url()),range:response.request().headers().range||null,status:response.status()});});
  if(kind==='peertube'){
    await page.goto(new URL('/login',manifest.remote).href);
    await page.locator('#username').fill(process.env.PEERTUBE_E2E_USER);
    await page.locator('#password').fill(process.env.PEERTUBE_E2E_PASSWORD);
    await page.locator('input[type="submit"][value="Login"]').click();await page.waitForURL(u=>u.pathname!=='/login');
  }
  if(delay)await page.route(segments,async route=>{
    const url = new URL(route.request().url());
    // Initialization is not peer-transferred. The native range proxy folds
    // EXT-X-MAP's byte range into s/l query parameters, not a Range header.
    const initialization = route.request().headers().range?.startsWith('bytes=0-') || url.searchParams.get('s') === '0';
    if(!initialization)await pause(delay);
    await route.continue().catch(error=>{if(!/already handled|has been closed/.test(String(error)))throw error;});});
  await page.goto(kind==='peertube'?manifest.remote:manifest.local,{waitUntil:'domcontentloaded'});
  if(kind==='peertube'){
    const consent=page.getByRole('button',{name:'OK',exact:true});await consent.waitFor({state:'visible',timeout:3000}).catch(()=>{});
    if(await consent.isVisible())await consent.click();
  } else if(noJS){if(await page.locator('.media__p2p-toggle').isVisible())throw new Error('no-JS inert peer choice');}
  else {
    const choice=page.locator('.media__p2p-toggle');await choice.waitFor({state:'visible'});
    if(!await choice.isChecked())throw new Error('sharing did not default on');
    if(off){await choice.uncheck();await page.reload();if(await choice.isChecked())throw new Error('opt-out not persistent');}
  }
  const video=page.locator(kind==='peertube'?'video.vjs-tech':'video[data-hls]').first();await video.waitFor({state:'attached'});
  if(kind==='peertube'&&await page.locator('.vjs-big-play-button').isVisible())await page.locator('.vjs-big-play-button').click();
  await video.evaluate(v=>{v.muted=true;v.loop=true;v.play().catch(()=>{});});
  if(!noJS){
    if(kind==='native'){
      const quality=page.getByRole('combobox',{name:'Video quality',exact:true});await quality.waitFor({state:'visible'});
      await quality.selectOption({label:'240p'});
    }else{
      await video.hover({force:true});await page.locator('.vjs-settings').click();
      await page.locator('.vjs-main-menu .vjs-menu-item').filter({hasText:'Quality'}).click();
      const menu=page.locator('.vjs-menu').filter({has:page.locator('.vjs-back-button').filter({hasText:'Quality'})});
      await menu.locator('[role="menuitemradio"]',{hasText:'240p'}).click();
    }
  }
  try {
    if (!deferDecode) {
    await until(async()=>{const s=await sample(video);return s.presented>=8&&s.time>.2&&!s.error&&(!noJS?s.height===240:true);},30,'actual decoder failed startup');
    }
  } catch(error) {
    result.failedStartup={kind,delay,off,noJS,sample:await sample(video),responses,errors,trackerContacts:sockets};
    await page.screenshot({path:join(evidence,`failed-startup-${kind}.png`)});
    throw error;
  }
  return{context,page,video,sockets,errors,responses,kind};
}
let seed,currentReceiver;
try{
  for(const option of [{noJS:true},{off:true}]){
    const client=await open('native',option);await pause(1500);
    if(client.sockets.length||await client.video.getAttribute('data-p2p-active'))throw new Error('no-JS/opt-out contacted tracker');
    result.phases.push({kind:option.noJS?'no-JS':'pre-contact-persistent-opt-out',sample:await sample(client.video),trackerContacts:0});
    await client.context.close();
  }
  seed=await open('native',{fault:true});
  await until(async()=>(await sample(seed.video)).time>12,30,'seed failed to buffer real movie');
  const peer=await open('peertube',{delay:1500});
  await until(async()=>(await sample(seed.video)).uploaded>0,45,'actual PeerTube did not receive native seeded bytes');
  result.phases.push({kind:'actual PeerTube cross-product recipient',nativeSeed:await sample(seed.video),peer:await sample(peer.video)});
  await peer.context.close();
  save('actual PeerTube exchange established');
  for(const mode of ['healthy','corrupt','stale']){
    const beforeWire=await seed.page.evaluate(()=>({...window.__ownedPeerWireFault}));
    // Fault BEFORE creating the cold receiver, not after seeking: the engine
    // prefetches beyond HTMLMediaElement.buffered and can satisfy that seek
    // without sending any new RTC bytes. The preceding healthy phase proves
    // this seed and the shipped receiver really exchange accepted segments.
    await seed.page.evaluate(mode=>{window.__ownedPeerWireFault.mode=mode;},mode);
    const receiver=await open('native',{delay:4000,deferDecode:mode!=='healthy'});
    currentReceiver=receiver;
    if(mode==='healthy')await until(async()=>(await sample(receiver.video)).validated>0,45,'receiver has no checksum-validated real peer segment');
    if(mode!=='healthy') {
      result.phases.push({mode,coldReceiverBeforeDecode:await sample(receiver.video)});save(`${mode}-cold-receiver`);
      await until(async()=>(await sample(receiver.video)).validationErrors>0,45,`${mode} raw peer bytes were not rejected by shipped checksum validator`);
      const wire=await seed.page.evaluate(()=>({...window.__ownedPeerWireFault}));
      if(wire.changedBytes<=beforeWire.changedBytes||wire.changedChunks<=beforeWire.changedChunks)
        throw new Error('validation failure lacked actual mutated RTC byte proof');
    }
    // Restore immediate native HTTP, not a fabricated player callback. The
    // receiving engine must recover and keep presenting decoded frames.
    await receiver.page.unrouteAll({behavior:'ignoreErrors'});
    const recoveryBegan=Date.now(),initial=await sample(receiver.video);
    await until(async()=>{const s=await sample(receiver.video);return !s.error&&s.presented-initial.presented>=15&&s.time>initial.time+.5;},20,'HTTP fallback did not resume real decoded frames');
    const fallbackMs=Date.now()-recoveryBegan,before=await sample(receiver.video),responsesBefore=receiver.responses.length;
    await pause(12000);const after=await sample(receiver.video),wire=await seed.page.evaluate(()=>({...window.__ownedPeerWireFault}));
    const phase={mode,beforeWire,wire,before,after,fallbackMs,successfulMediaResponses:receiver.responses.length,
      laterMediaResponses:receiver.responses.length-responsesBefore,errors:receiver.errors,sockets:receiver.sockets};result.phases.push(phase);save(`${mode}-decoded`);
    if(after.error||receiver.errors.length||after.presented-before.presented<120||after.time-before.time<6||!receiver.responses.length)
      throw new Error(`peer ${mode} fallback decode budget failed: ${JSON.stringify(phase)}`);
    if(mode==='healthy'&&after.validationErrors)throw new Error('healthy peer lane failed integrity');
    await receiver.page.screenshot({path:join(evidence,`peer-${mode}-native-320px.png`)});
    await receiver.context.close();
  }
  result.status='passed';save('complete');
}catch(error){result.status='failed';result.error=String(error);
  if(seed){result.seed=await sample(seed.video).catch(()=>null);result.actualWire=await seed.page.evaluate(()=>({...window.__ownedPeerWireFault})).catch(()=>null);}
  if(currentReceiver)result.failedReceiver={sample:await sample(currentReceiver.video).catch(()=>null),responses:currentReceiver.responses,errors:currentReceiver.errors};
  save('failed');throw error;}
finally{for(const context of contexts)await context.close().catch(()=>{});await browser.close();}
