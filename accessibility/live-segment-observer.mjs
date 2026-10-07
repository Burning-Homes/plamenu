import {writeFileSync} from 'node:fs';
import {join} from 'node:path';
import {createHash} from 'node:crypto';

// Read only already-completed owned playback responses. The optional probe
// issues no requests and retains neither URLs nor headers nor credentials.
export function observeLiveSegments(page,evidence,broadcast,isSteady) {
  let reserved=0,playlists=0;
  const pending=[],rows=[];
  page.on('response',response=>{
    if(!isSteady()||response.status()!==200)return;
    const path=new URL(response.url()).pathname;
    let originFileNameSha256=null;
    try {
      const key=path.match(/\/live\/([A-Za-z0-9_-]+)\.(?:ts|m3u8)$/)?.[1];
      if(key){
        const origin=new URL(Buffer.from(key,'base64url').toString('utf8'));
        originFileNameSha256=createHash('sha256').update(origin.pathname.split('/').at(-1)).digest('hex');
      }
    } catch {}
    const length=Number(response.headers()['content-length']);
    if(!Number.isInteger(length)||length<1||length>2*1024*1024)return;
    if(playlists<12&&length<=64*1024&&/^\/media\/hls\/\d+\/live\/[A-Za-z0-9_-]+\.m3u8$/.test(path)){
      const ordinal=playlists++,headersAt=Date.now();
      pending.push((async()=>{
        try {
          const bytes=await response.body(),completedAt=Date.now();
          const text=bytes.toString('utf8');
          const sequence=text.match(/^#EXT-X-MEDIA-SEQUENCE:(\d+)/m);
          const target=text.match(/^#EXT-X-TARGETDURATION:(\d+)/m);
          if(!sequence||!target)return;
          const durations=[...text.matchAll(/^#EXTINF:([\d.]+)/gm)].map(match=>Number(match[1]));
          const segmentUriHashes=text.split(/\r?\n/).filter(line=>line&&line[0]!=='#')
            .map(uri=>createHash('sha256').update(new URL(uri,response.url()).href).digest('hex'));
          rows.push({broadcast,ordinal,kind:'playlist',originFileNameSha256,headersAtMs:headersAt,completedAtMs:completedAt,
            requestStartMs:response.request().timing().startTime,
            sequence:Number(sequence[1]),targetDurationSeconds:Number(target[1]),
            durationsSeconds:durations,segmentUriHashes,bytes:bytes.length});
        } catch {rows.push({broadcast,ordinal,kind:'playlist',unavailable:true});}
      })());
      return;
    }
    if(reserved>=12||!/^\/media\/hls\/\d+\/live\/[A-Za-z0-9_-]+\.ts$/.test(path))return;
    const ordinal=reserved++,headersAt=Date.now();
    pending.push((async()=>{
      try {
        const bytes=await response.body(),completedAt=Date.now();
        if(bytes.length!==length)return;
        const name=`live-response-${broadcast}-${ordinal}.ts`;
        writeFileSync(join(evidence,name),bytes,{mode:0o600,flag:'wx'});
        rows.push({broadcast,ordinal,name,originFileNameSha256,headersAtMs:headersAt,completedAtMs:completedAt,
          bytes:bytes.length,sha256:createHash('sha256').update(bytes).digest('hex'),
          requestUriSha256:createHash('sha256').update(response.url()).digest('hex')});
      } catch {rows.push({broadcast,ordinal,unavailable:true});}
    })());
  });
  return async()=>{await Promise.allSettled(pending);return rows;};
}
