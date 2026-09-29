// Keep an actual old first-party seeder alive across a stable-UUID replacement.
// Only disposable local fixture credentials are accepted through the environment.
import {chromium} from 'playwright';
const [watchUrl,candidate]=process.argv.slice(2);
const authorization=process.env.AP09_REPLACEMENT_AUTH;
if(!watchUrl||!candidate||!authorization)throw new Error('expected local watch URL, private candidate and fixture authorization');
const browser=await chromium.launch();
async function observePeerFailures(page){
  await page.addInitScript(()=>{
    window.__replacementPeerFailures=[];
    let player;
    Object.defineProperty(window,'Hls',{configurable:true,get:()=>player,set:Hls=>{
      player=class extends Hls {
        constructor(...args){
          super(...args);
          queueMicrotask(()=>this.p2pEngine?.addEventListener('onSegmentError',event=>{
            window.__replacementPeerFailures.push({type:event.error?.type,cause:event.error?.cause?.type,source:event.downloadSource,
              externalId:event.segment?.externalId,message:event.error?.message});
          }));
        }
      };
    }});
  });
}
async function start(page){
  const video=page.locator('video[data-hls]');
  await video.waitFor({state:'attached'});
  await video.evaluate(v=>{v.muted=true;v.loop=true;v.play().catch(()=>{});});
  const quality=page.getByRole('combobox',{name:'Video quality',exact:true});
  await quality.waitFor({state:'visible',timeout:20000});
  await quality.selectOption({label:'480p'});
  await page.waitForFunction(()=>document.querySelector('video[data-hls]')?.videoHeight===480,null,{timeout:30000});
  await page.waitForFunction(()=>document.querySelector('video[data-hls]')?.currentTime>.5,null,{timeout:30000});
  return video;
}
try{
  const seedContext=await browser.newContext({ignoreHTTPSErrors:true});
  const seed=await seedContext.newPage();
  await seed.goto(watchUrl);
  const old=await start(seed);
  await seed.waitForFunction(()=>{
    const v=document.querySelector('video[data-hls]');
    return v?.dataset.p2pInfohash && v.buffered.length && v.buffered.end(v.buffered.length-1)>=v.duration-.2;
  },null,{timeout:30000});
  const oldHash=await old.getAttribute('data-p2p-infohash');
  const media=new URL(await old.getAttribute('data-hls')).pathname.split('/')[3];
  const api=new URL(`/api/v1/videos/${media}/content`,watchUrl).href;
  const applied=await seed.request.put(api,{headers:{Authorization:authorization},data:{candidate_media_id:candidate,revision:0,confirm:true}});
  if(applied.status()!==200)throw new Error(`replacement refused: ${applied.status()} ${await applied.text()}`);
  const freshContext=await browser.newContext({ignoreHTTPSErrors:true});
  const fresh=await freshContext.newPage();
  await observePeerFailures(fresh);
  await fresh.route('**/media/*.m4s*',async route=>{
    if(!route.request().headers().range?.startsWith('bytes=0-'))await new Promise(resolve=>setTimeout(resolve,8000));
    await route.continue();
  });
  await fresh.goto(watchUrl);
  const current=await start(fresh);
  if(!(await current.getAttribute('data-hls')).includes('rev=1'))throw new Error('new playback metadata was not revisioned');
  await fresh.waitForFunction(()=>Number(document.querySelector('video[data-hls]')?.dataset.p2pPeers||0)>0,null,{timeout:20000});
  if(await current.getAttribute('data-p2p-infohash')!==oldHash)throw new Error('replacement changed the PeerTube-compatible UUID swarm');
  // The first fragment is urgent HTTP traffic. Observe the later peer
  // requests, not merely the first decoded frame before ICE discovery.
  await fresh.waitForFunction(()=>window.__replacementPeerFailures.some(e=>
    e.source==='p2p' && /bytes-length-mismatch|validation-failed/.test(`${e.type||''} ${e.cause||''}`)),
    null,{timeout:30000}).catch(()=>{});
  const state=await current.evaluate(v=>{
    const canvas=document.createElement('canvas');canvas.width=1;canvas.height=1;
    const ctx=canvas.getContext('2d');ctx.drawImage(v,0,0,1,1);
    return {dataset:{...v.dataset},pixel:[...ctx.getImageData(0,0,1,1).data],frames:v.getVideoPlaybackQuality().totalVideoFrames,time:v.currentTime,
      peerFailures:window.__replacementPeerFailures};
  });
  const oldUploaded=Number(await old.getAttribute('data-p2p-uploaded')||0);
  // Different-length stale fragments are refused at the peer protocol's
  // size declaration, before payload transfer. Equal-length corruption reaches
  // the mandatory checksum validator, covered by shipped-function unit tests.
  const rejected=state.peerFailures?.filter(e=>e.source==='p2p' && /bytes-length-mismatch|validation-failed/.test(`${e.type||''} ${e.cause||''}`))||[];
  if(!rejected.length)throw new Error(`no stale peer size/checksum refusal observed: ${JSON.stringify({state,oldUploaded})}`);
  if(Number(state.dataset.p2pValidated||0))throw new Error(`old film bytes passed the new checksum document: ${JSON.stringify(state)}`);
  if(!state.frames || state.pixel[0]>30 || state.pixel[1]>30 || state.pixel[2]<200)throw new Error(`replacement decoder rendered old or invalid content: ${JSON.stringify(state)}`);
  console.log(JSON.stringify({oldInfohash:oldHash,oldBytesOffered:oldUploaded,oldBytesReceived:Number(state.dataset.p2pDownloaded||0),stalePeerRefusals:rejected,
    rejectedChecksums:Number(state.dataset.p2pValidationErrors||0),validatedOldBytes:0,replacementPixel:state.pixel,decodedFrames:state.frames}));
}finally{await browser.close();}
