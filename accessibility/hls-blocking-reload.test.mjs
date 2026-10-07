import assert from 'node:assert/strict';
import {createRequire} from 'node:module';
import {test} from 'node:test';
const Hls=createRequire(import.meta.url)('../crates/server/src/web/assets/hls.min.js');
globalThis.self=globalThis;
function verify({sequence=0,count=6,blocking=true,ended=false,event=true,playlistUrl='https://owned.invalid/media/live/encoded.m3u8'}={}) {
  const h=new Hls({autoStartLoad:false,enableWorker:false,lowLatencyMode:true});
  assert.equal(Hls.version,'1.6.16');
  const parser=h.networkControllers[0],controller=h.levelController,original=parser.hls;
  const playlist={uri:playlistUrl,attrs:{}};
  let details;
  const stats={loading:{start:performance.now(),first:performance.now(),end:performance.now()},parsing:{start:0,end:0}};
  const body=['#EXTM3U','#EXT-X-VERSION:3','#EXT-X-TARGETDURATION:4',
    `#EXT-X-MEDIA-SEQUENCE:${sequence}`,event?'#EXT-X-PLAYLIST-TYPE:EVENT':'',
    blocking?'#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES':'',
    ...Array.from({length:count},(_,i)=>`#EXTINF:4.0,\nsegment-${sequence+i}.ts`),ended?'#EXT-X-ENDLIST':''].join('\n');
  try {
    parser.hls={levels:[playlist],config:h.config,logger:h.logger,
      trigger:(name,data)=>{assert.equal(name,Hls.Events.LEVEL_LOADED);details=data.details;}};
    parser.handleTrackOrLevelPlaylist({data:body,url:playlist.uri},stats,
      {id:0,level:0,type:'level',url:playlist.uri,levelOrTrack:playlist,deliveryDirectives:null},null,null);
    parser.hls=original;
    const calls=[];
    controller.canLoad=true;
    controller.loadingPlaylist=(_playlist,directives)=>calls.push({mode:'immediate',msn:directives?.msn,part:directives?.part,url:directives?.addDirectives(playlist.uri)});
    controller.scheduleLoading=(_playlist,directives)=>calls.push({mode:'scheduled',msn:directives?.msn});
    controller.playlistLoaded(0,{details,stats,levelInfo:playlist});
    return {calls,details};
  } finally {parser.hls=original;h.destroy();}
}
test('vendored HLS requests the next closed segment immediately for EVENT history without parts',()=>{
  const {calls,details}=verify();assert.equal(details.startSN,0);assert.equal(details.endSN,5);
  assert.equal(details.lastPartIndex,-1);assert.equal(details.partList,null);
  assert.equal(calls.length,1);assert.equal(calls[0].mode,'immediate');assert.equal(calls[0].msn,6);
  assert.equal(calls[0].part,undefined);assert.equal(new URL(calls[0].url).searchParams.get('_HLS_msn'),'6');
  assert.equal(new URL(calls[0].url).searchParams.has('_HLS_part'),false);
});
test('sliding-window requests use sequence plus segment count',()=>{
  const {calls,details}=verify({sequence:100,event:false});assert.equal(details.endSN,105);assert.equal(calls[0].msn,106);
});
test('ordinary HLS remains scheduled when blocking is absent',()=>{
  assert.deepEqual(verify({blocking:false}).calls,[{mode:'scheduled',msn:undefined}]);
});
test('ended history does not schedule another reload',()=>{
  assert.equal(verify({ended:true}).calls.length,0);
});
test('the very first zero-numbered closed segment still uses ordinary startup scheduling',()=>{
  assert.deepEqual(verify({count:1}).calls,[{mode:'scheduled',msn:undefined}]);
});

test('the actual vendored master parser keeps alternative audio and variant URLs for a consistently advertised ladder',()=>{
  const h=new Hls({autoStartLoad:false,enableWorker:false,lowLatencyMode:true});
  const parser=h.networkControllers[0],original=parser.hls;
  const url='https://owned.invalid/media/hls/123/master.m3u8';
  const data=['#EXTM3U','#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="a",NAME="Audio",DEFAULT=YES,AUTOSELECT=YES,URI="live/audio.m3u8"',
    '#EXT-X-STREAM-INF:BANDWIDTH=1000,AUDIO="a"','live/low.m3u8',
    '#EXT-X-STREAM-INF:BANDWIDTH=2000,AUDIO="a"','live/high.m3u8'].join('\n');
  let loaded;
  try {
    parser.hls={config:h.config,logger:h.logger,trigger:(event,details)=>{assert.equal(event,Hls.Events.MANIFEST_LOADED);loaded=details;}};
    parser.handleMasterPlaylist({data,url},{loading:{start:0,first:0,end:0}}, {type:'manifest',url},null);
    assert.equal(loaded.levels.length,2);assert.equal(loaded.audioTracks.length,1);
    assert.deepEqual(loaded.levels.map(l=>new URL(l.url).pathname),['/media/hls/123/live/low.m3u8','/media/hls/123/live/high.m3u8']);
    assert.equal(new URL(loaded.audioTracks[0].url).pathname,'/media/hls/123/live/audio.m3u8');
    for(const child of loaded.levels.map(l=>l.url).concat(loaded.audioTracks.map(t=>t.url))) {
      const {calls,details}=verify({playlistUrl:child});
      assert.equal(details.canBlockReload,true);assert.equal(calls[0].msn,6);
      assert.equal(new URL(calls[0].url).pathname,new URL(child).pathname);
      assert.equal(new URL(calls[0].url).searchParams.get('_HLS_msn'),'6');
    }
  } finally {parser.hls=original;h.destroy();}
});
