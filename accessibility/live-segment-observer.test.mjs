import assert from 'node:assert/strict';
import {test} from 'node:test';
import {EventEmitter} from 'node:events';
import {mkdtempSync,readFileSync,statSync,rmSync} from 'node:fs';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
import {observeLiveSegments} from './live-segment-observer.mjs';

test('live response probe retains twelve existing bodies with private permissions and no URI metadata',async()=>{
  const dir=mkdtempSync(join(tmpdir(),'plamenu-live-observer-')),page=new EventEmitter();
  let reads=0,steady=false;
  const response={status:()=>200,url:()=> 'https://owned/media/hls/123/live/abc.ts?private=secret',
    headers:()=>({'content-length':'16','authorization':'secret'}),body:async()=>{reads++;return Buffer.alloc(16,7);}};
  try {
    const finish=observeLiveSegments(page,dir,0,()=>steady);
    page.emit('response',response);assert.equal(reads,0);
    steady=true;for(let n=0;n<20;n++)page.emit('response',response);
    const rows=await finish();assert.equal(reads,12);assert.equal(rows.length,12);
    assert.ok(!JSON.stringify(rows).includes('secret'));assert.ok(!JSON.stringify(rows).includes('https'));
    for(const row of rows){const path=join(dir,row.name);assert.equal(statSync(path).mode&0o777,0o600);assert.equal(readFileSync(path).length,16);}
  } finally {rmSync(dir,{recursive:true});}
});

test('live playlist probe records numeric freshness without retaining segment URIs',async()=>{
  const page=new EventEmitter(),body=Buffer.from('#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:18\n#EXTINF:4.0,\nhttps://private/secret.ts\n');
  const finish=observeLiveSegments(page,'/unused',0,()=>true);
  page.emit('response',{status:()=>200,url:()=> 'https://owned/media/hls/1/live/abc.m3u8',
    headers:()=>({'content-length':String(body.length)}),body:async()=>body,
    request:()=>({timing:()=>({startTime:42})})});
  const rows=await finish();assert.equal(rows.length,1);assert.equal(rows[0].sequence,18);
  assert.equal(rows[0].segmentUriHashes.length,1);assert.match(rows[0].segmentUriHashes[0],/^[a-f0-9]{64}$/);
  assert.deepEqual(rows[0].durationsSeconds,[4]);assert.equal(rows[0].requestStartMs,42);
  assert.ok(!JSON.stringify(rows).includes('secret'));assert.ok(!JSON.stringify(rows).includes('https'));
});

test('live response probe rejects unrelated, oversized and unknown-length responses without reading bodies',async()=>{
  const page=new EventEmitter();let reads=0;
  const finish=observeLiveSegments(page,'/unused',1,()=>true);
  for(const [path,length,status] of [['/other.ts','10',200],['/media/hls/1/live/a.ts',undefined,200],
    ['/media/hls/1/live/a.ts',String(2*1024*1024+1),200],['/media/hls/1/live/a.ts','10',404]]){
    page.emit('response',{status:()=>status,url:()=>`https://owned${path}`,headers:()=>({'content-length':length}),
      body:async()=>{reads++;throw Error('must not read');}});
  }
  assert.equal(reads,0);assert.equal((await finish()).length,0);
});
