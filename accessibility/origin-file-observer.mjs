import {lstatSync,readdirSync,readFileSync} from 'node:fs';
import {join} from 'node:path';
import {createHash} from 'node:crypto';

const digest=name=>createHash('sha256').update(name).digest('hex');
export function observeOriginDirectory(root,uuid) {
  if(!/^[a-f0-9]{8}-(?:[a-f0-9]{4}-){3}[a-f0-9]{12}$/.test(uuid))throw Error('invalid owned UUID');
  const dir=join(root,uuid),rows=[];
  try {
    if(!lstatSync(dir).isDirectory()||lstatSync(dir).isSymbolicLink())return rows;
    const names=readdirSync(dir).filter(name=>/^[A-Za-z0-9._-]+\.m3u8$/.test(name)).slice(0,16);
    for(const name of names){
      const file=join(dir,name),before=lstatSync(file);
      if(!before.isFile()||before.isSymbolicLink()||before.size>64*1024)continue;
      const text=readFileSync(file,'utf8'),after=lstatSync(file);
      if(before.mtimeMs!==after.mtimeMs||before.size!==after.size)continue;
      const sequence=text.match(/^#EXT-X-MEDIA-SEQUENCE:(\d+)/m);
      if(!sequence)continue;
      const segments=text.split(/\r?\n/).filter(line=>/^[A-Za-z0-9._-]+\.ts$/.test(line));
      if(!segments.length)continue;
      const latest=segments.at(-1),stat=lstatSync(join(dir,latest));
      if(!stat.isFile()||stat.isSymbolicLink())continue;
      rows.push({playlistNameSha256:digest(name),playlistMtimeMs:after.mtimeMs,
        sequence:Number(sequence[1]),segmentCount:segments.length,
        latestNameSha256:digest(latest),latestMtimeMs:stat.mtimeMs,latestBytes:stat.size});
    }
  } catch {return rows;}
  return rows;
}

if(process.argv[2]==='--owned-origin-observation'){
  console.log(JSON.stringify({atMs:Date.now(),broadcasts:process.argv.slice(3).map((uuid,broadcast)=>({
    broadcast,playlists:observeOriginDirectory('/data/streaming-playlists/hls',uuid)}))}));
}
