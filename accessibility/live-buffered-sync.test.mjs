import assert from 'node:assert/strict';
import {test} from 'node:test';
import {readFileSync} from 'node:fs';
import {createRequire} from 'node:module';
import {runInNewContext} from 'node:vm';

const require=createRequire(import.meta.url);
const Hls=require('../crates/server/src/web/assets/hls.min.js');
const source=readFileSync(new URL('../crates/server/src/web/assets/app.js',import.meta.url),'utf8');
const start=source.indexOf('const hlsConfig = live');
const end=source.indexOf('// The peer module',start);
assert.ok(start>=0&&end>start);
const config=runInNewContext(`${source.slice(start,end)};hlsConfig`,{live:true});
const catchUpStart=source.indexOf('function catchUpBufferedLive(');
const catchUpEnd=source.indexOf('function initHlsVideo(',catchUpStart);
assert.ok(catchUpStart>=0&&catchUpEnd>catchUpStart);
const catchUp=runInNewContext(`(${source.slice(catchUpStart,catchUpEnd).trim()})`);

function synchronize(ranges,{mode=config.liveSyncMode,current=69,sync=72,start=48,edge=76}={}) {
  const player=new Hls({...config,liveSyncMode:mode,autoStartLoad:false,enableWorker:false});
  const controller=player.streamController,original=controller.hls;
  const media={currentTime:current,duration:200,readyState:4,
    buffered:{length:ranges.length,start:n=>ranges[n][0],end:n=>ranges[n][1]}};
  try {
    controller.hls={liveSyncPosition:sync,hasEnoughToStart:true};controller.media=media;controller._hasEnoughToStart=true;
    controller.synchronizeToLiveEdge({fragmentStart:start,edge,targetduration:4});
    return media.currentTime;
  } finally {controller.media=null;controller.hls=original;player.destroy();}
}

test('real vendored HLS reproduces the empty-boundary catch-up from the observed live timeline',()=>{
  assert.equal(synchronize([[48,72]],{mode:'edge'}),72);
});

test('production live settings keep playing while the advertised target is not buffered',()=>{
  assert.equal(synchronize([[48,72]]),69);
  assert.equal(config.liveSyncDuration,4);assert.equal(config.liveMaxLatencyDuration,5);
});

test('catch-up resumes as soon as the target has a playable buffer',()=>{
  assert.equal(synchronize([[48,76]]),72);
});

test('a later buffered range permits recovery beyond an expired sliding window',()=>{
  assert.equal(synchronize([[48,72],[100,104]],{sync:100,start:100,edge:108}),100);
});

function liveState(end=72) {
  return {currentTime:69,paused:false,readyState:4,
    buffered:{length:1,start:()=>48,end:()=>end}};
}
const livePlayer={liveSyncPosition:72,latestLevelDetails:{edge:76},config};

test('late live bytes trigger the previously deferred safe catch-up once',()=>{
  const video=liveState();assert.equal(catchUp(video,livePlayer,false),false);assert.equal(video.currentTime,69);
  video.buffered.end=()=>76;
  assert.equal(catchUp(video,livePlayer,false),true);assert.equal(video.currentTime,72);
  video.currentTime=76;video.buffered.end=()=>80;
  assert.equal(catchUp(video,{...livePlayer,liveSyncPosition:76,latestLevelDetails:{edge:80}},false),false);
});

test('post-append live catch-up preserves explicit DVR rewind and paused playback',()=>{
  const video=liveState(76);
  assert.equal(catchUp(video,livePlayer,true),false);assert.equal(video.currentTime,69);
  video.paused=true;assert.equal(catchUp(video,livePlayer,false),false);assert.equal(video.currentTime,69);
});

test('post-append live catch-up leaves small timing jitter alone and avoids unready media',()=>{
  const video=liveState(76);video.currentTime=71.8;
  assert.equal(catchUp(video,livePlayer,false),false);assert.equal(video.currentTime,71.8);
  video.currentTime=69;video.readyState=1;
  assert.equal(catchUp(video,livePlayer,false),false);assert.equal(video.currentTime,69);
});

test('post-append live catch-up does not retain an extra second behind the selected target',()=>{
  const video=liveState(76);video.currentTime=71.1;
  assert.equal(catchUp(video,livePlayer,false),true);assert.equal(video.currentTime,72);
  assert.equal(config.liveSyncDuration,4);assert.equal(config.liveMaxLatencyDuration,5);
});
