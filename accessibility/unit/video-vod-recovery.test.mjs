import {readFileSync} from 'node:fs';
import {runInNewContext} from 'node:vm';
import {test} from 'node:test';
import assert from 'node:assert/strict';

// Exercise the shipped recovery closures with deterministic clocks/DOM/media
// state. Browser journeys additionally decode real media after HTTP faults.
const source=readFileSync(new URL('../../crates/server/src/web/assets/app.js',import.meta.url),'utf8');
const code=source.slice(source.indexOf("    const liveVideo = video.dataset.live"),source.indexOf('    const showLiveFailure ='));
function fixture({live=false,paused=false,buffered=true,frames=0}={}){
  let now=0,id=0,destroyed=0,played=0,reloaded=0;
  const timers=new Map(),events=new Map(),notices=[];
  const sharing={checked:true,disabled:false};
  const quality={removed:false,remove(){this.removed=true;}};
  const figure={querySelector:selector=>selector==='.video-quality'?quality:
    selector==='.media__live-recovery'?notices[0]||null:
    selector==='.media__live-recovery a'?notices[0]?.children[0]?.children[0]||null:null,
    appendChild:child=>notices.push(child)};
  const video={dataset:{live:live?'live':undefined,p2pActive:'true'},currentTime:.25,
    duration:12,paused,readyState:buffered?4:0,isConnected:true,buffered:{length:buffered?1:0},
    getVideoPlaybackQuality:()=>({totalVideoFrames:frames}),
    closest:()=>figure,focus(){},play(){played++;this.paused=false;return Promise.resolve();},
    load(){reloaded++;for(const fn of events.get('loadedmetadata')||[])fn();},
    addEventListener(name,fn){if(!events.has(name))events.set(name,[]);events.get(name).push(fn);}};
  const document={hidden:false,activeElement:null,createElement:tag=>({tag,children:[],
    setAttribute(name,value){this[name]=value;},appendChild(child){this.children.push(child);}})};
  const api=runInNewContext(`let hlsPlayer=engine;${code};({watchVodStart,standardVideo,started(){adaptiveStarted=true;}})`,{
    hlsCleanup:new WeakMap(),retired:false,original:null,originalFallback:false,profiling:false,
    video,document,choice:sharing,progressive:'https://local.test/media/film.mp4',engine:{destroy(){destroyed++;}},
    reloadExternalTracks(){},performance:{now:()=>now},
    setTimeout(fn){const task=++id;timers.set(task,fn);return task;},
    clearTimeout(task){timers.delete(task);}});
  return {api,video,document,timers,sharing,quality,notices,
    advance(ms){now=ms;const tasks=[...timers.values()];timers.clear();for(const fn of tasks)fn();},
    error(){for(const fn of events.get('error')||[])fn();},
    state:()=>({destroyed,played,reloaded})};
}
test('buffered zero-frame decoder stall falls back once and stops peer engine',()=>{
  const f=fixture();f.api.watchVodStart();f.advance(4999);assert.equal(f.state().destroyed,0);
  f.advance(5001);assert.equal(f.state().destroyed,1);assert.equal(f.video.src,'https://local.test/media/film.mp4');
  assert.equal(f.video.currentTime,.25);assert.equal(f.video.dataset.p2pActive,undefined);
  assert.equal(f.sharing.checked,false);assert.equal(f.sharing.disabled,true);assert.equal(f.quality.removed,true);
  assert.equal(f.notices.length,1);assert.equal(f.notices[0].role,'status');assert.equal(f.timers.size,0);
  f.api.standardVideo();assert.equal(f.state().destroyed,1);assert.equal(f.notices.length,1);
});
test('slow unbuffered startup gets fifteen seconds, not the decoder budget',()=>{
  const f=fixture({buffered:false});f.api.watchVodStart();f.advance(5001);assert.equal(f.state().destroyed,0);
  f.advance(14999);assert.equal(f.state().destroyed,0);f.advance(15001);assert.equal(f.state().destroyed,1);
});
test('late first bytes get a full decoder grace period within the total budget',()=>{
  const f=fixture({buffered:false});f.api.watchVodStart();f.advance(8000);
  f.video.readyState=4;f.video.buffered.length=1;f.advance(8500);
  assert.equal(f.state().destroyed,0);f.advance(13000);assert.equal(f.state().destroyed,0);
  f.advance(13501);assert.equal(f.state().destroyed,1);
});
test('decoded progress ends the startup watchdog without fallback',()=>{
  const f=fixture({frames:2});f.api.watchVodStart();f.video.currentTime=1;f.advance(500);
  assert.equal(f.state().destroyed,0);assert.equal(f.timers.size,0);
});
test('paused, hidden and detached videos do not restart automatically',()=>{
  for(const change of [f=>{f.video.paused=true;},f=>{f.document.hidden=true;},f=>{f.video.isConnected=false;}]){
    const f=fixture();f.api.watchVodStart();change(f);f.advance(16000);
    assert.equal(f.state().destroyed,0);assert.equal(f.state().played,0);assert.equal(f.timers.size,0);
  }
});
test('live recovery and pre-interaction errors are never converted to VOD',()=>{
  const live=fixture({live:true});live.api.watchVodStart();live.api.standardVideo();
  assert.equal(live.timers.size,0);assert.equal(live.state().destroyed,0);
  const f=fixture();f.error();assert.equal(f.state().destroyed,0);
  f.api.started();f.error();assert.equal(f.state().destroyed,1);
});
