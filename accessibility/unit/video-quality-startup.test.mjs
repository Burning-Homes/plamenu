import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {createRequire} from 'node:module';
import {runInNewContext} from 'node:vm';
import {test} from 'node:test';

globalThis.self=globalThis;
globalThis.window=globalThis;
const require=createRequire(import.meta.url);
const Hls=require('../../crates/server/src/web/assets/hls.min.js');
const source=readFileSync(new URL('../../crates/server/src/web/assets/app.js',import.meta.url),'utf8');
const code=source.slice(source.indexOf('  function buildQualityMenu('),source.indexOf('  const hlsCleanup'));
function fixture(buffered){
  const hls=new Hls({autoStartLoad:false,enableWorker:false});
  // Two inert real SDK levels; no loader or MediaSource is attached.
  hls.levelController._levels=[240,480].map(height=>({height,width:height*2,bitrate:height*1000,
    url:['https://owned.invalid/'+height+'.m3u8'],attrs:{},loadError:0,fragmentError:0}));
  let change,select,flushes=0;
  hls.on(Hls.Events.BUFFER_FLUSHING,()=>flushes++);
  const figure={querySelector:()=>null,appendChild:node=>{select=node;}};
  const video={readyState:buffered?4:0,buffered:{length:buffered?1:0},closest:()=>figure,
    getAttribute:()=>'/master.m3u8'};
  const document={createElement:()=>({children:[],setAttribute(){},querySelector:()=>null,
    appendChild(node){this.children.push(node);},addEventListener(event,fn){if(event==='change')change=fn;}})};
  runInNewContext(`${code};buildQualityMenu(video,hls,Hls)`,{video,hls,Hls,document});
  return{hls,choose(value){select.value=value;change();},flushes:()=>flushes};
}
test('manual quality before the first decoded buffer does not flush initial tracks',()=>{
  const f=fixture(false);
  try{f.choose('1');assert.equal(f.hls.manualLevel,1);assert.equal(f.flushes(),0);}
  finally{f.hls.destroy();}
});
test('automatic quality during unbuffered startup does not abort the initial fragment',()=>{
  const f=fixture(false);
  try{f.choose('-1');assert.equal(f.hls.manualLevel,-1);assert.equal(f.flushes(),0);}
  finally{f.hls.destroy();}
});
test('quality changes after buffering still switch immediately',()=>{
  const f=fixture(true);
  try{f.choose('1');assert.equal(f.hls.manualLevel,1);assert.ok(f.flushes()>0);}
  finally{f.hls.destroy();}
});
