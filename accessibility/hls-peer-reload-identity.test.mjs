import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {createRequire} from 'node:module';
import {runInNewContext} from 'node:vm';
import {webcrypto} from 'node:crypto';
import {test} from 'node:test';

globalThis.self=globalThis;
globalThis.window=globalThis;
const require=createRequire(import.meta.url);
const Hls=require('../crates/server/src/web/assets/hls.min.js');
const Engine=require('../crates/server/src/web/assets/p2p-media-loader-hlsjs.min.js').p2pml.hlsjs.HlsJsP2PEngine;
const source=readFileSync(new URL('../crates/server/src/web/assets/app.js',import.meta.url),'utf8');
const functions=source.slice(source.indexOf('  function peerSegmentValidator('),source.indexOf('  function loadHls('));

async function verify(type,canonical,directives) {
  const video={dataset:{p2pUuid:'owned',p2pSwarm:'owned-swarm',p2pSha256:'/hashes',p2pTrackers:'["wss://owned.invalid/tracker"]'}};
  const options=await runInNewContext(`${functions}; peerOptions(video,true)`,{
    video,peerSharingEnabled:()=>true,peerHashes:async()=>({}),
    window:{RTCPeerConnection:{},crypto:webcrypto},crypto:webcrypto,navigator:{},
    location:{href:'https://owned.invalid/'},URL,Uint8Array,TextDecoder,atob,setTimeout,
  });
  const h=new Hls({autoStartLoad:false,enableWorker:false,liveSyncDuration:4,maxBufferLength:30});
  // Isolate the actual peer event receivers from playback-only controllers.
  // The real parser and peer stream/segment inventory remain in use.
  for(const event of [Hls.Events.MANIFEST_LOADED,Hls.Events.LEVEL_UPDATED,Hls.Events.AUDIO_TRACK_LOADED]) h.removeAllListeners(event);
  const engine=new Engine({...options,core:{...options.core,isP2PDisabled:true}});
  h.p2pEngine=engine;
  engine.bindHls(h);
  options.onHlsJsCreated(h);
  engine.initHlsEvents();
  const level={url:[canonical],width:640,height:360,bitrate:1000,attrs:{}};
  const audio={url:canonical,name:'Audio',audioCodec:'mp4a.40.2'};
  h.trigger(Hls.Events.MANIFEST_LOADED,{url:'https://owned.invalid/master.m3u8',
    levels:type==='main'?[level]:[],audioTracks:type==='audio'?[audio]:[]});
  const parser=h.networkControllers[0],original=parser.hls;
  function append(count,requestUrl) {
    let details;
    parser.hls={levels:[level],config:h.config,logger:h.logger,
      trigger:(_event,data)=>{details=data.details;}};
    const body=['#EXTM3U','#EXT-X-VERSION:6','#EXT-X-TARGETDURATION:4',
      '#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES','#EXT-X-MEDIA-SEQUENCE:0',
      '#EXT-X-PLAYLIST-TYPE:EVENT',...Array.from({length:count},(_,n)=>`#EXTINF:4,\n${n}.ts`)].join('\n');
    const context={id:0,level:0,type:'level',url:requestUrl,levelOrTrack:{uri:canonical,attrs:{}},deliveryDirectives:null};
    const stats={loading:{start:0,first:0,end:0},parsing:{start:0,end:0}};
    parser.handleTrackOrLevelPlaylist({data:body,url:requestUrl},stats,context,null,null);
    parser.hls=original;
    assert.equal(details.url,requestUrl,'the actual parser retains the delivery request URL');
    const fragmentUrl=details.fragments.at(-1).url;
    h.trigger(type==='main'?Hls.Events.LEVEL_UPDATED:Hls.Events.AUDIO_TRACK_LOADED,{details,level:0,id:0});
    assert.equal(details.url,canonical,'peer metadata keeps the exact canonical query bytes');
    assert.equal(context.url,requestUrl,'the actual delivery request remains unchanged');
    assert.equal(details.fragments.at(-1).url,fragmentUrl,'segment addressing remains unchanged');
    assert.equal(engine.core.getStreamSegmentRuntimeIds(canonical).size,count);
    assert.equal(engine.core.hasSegment(fragmentUrl),true,'the new complete segment participates in peer delivery');
  }
  try {
    append(6,canonical);
    append(7,canonical+(canonical.includes('?')?'&':'?')+directives);
    append(8,canonical+(canonical.includes('?')?'&':'?')+'_HLS_msn=7');
  } finally {parser.hls=original;h.destroy();}
}

test('actual main peer inventory grows after each blocking reload',()=>verify('main',
  'https://owned.invalid/media/hls/123/live/encoded.m3u8','_HLS_msn=6'));
test('actual alternative audio inventory grows without changing authentication query encoding',()=>verify('audio',
  'https://owned.invalid/media/hls/123/live/audio.m3u8?auth=a%2fb%20c&u=encoded',
  '_HLS_msn=6&_HLS_part=0&_HLS_skip=YES'));
test('all duplicated delivery directives are removed while unrelated parameters survive',()=>verify('main',
  'https://owned.invalid/media/hls/123/live/encoded.m3u8?ordinary=one&ordinary=two',
  '_HLS_msn=5&%5FHLS_msn=6&_HLS_skip=v2'));
