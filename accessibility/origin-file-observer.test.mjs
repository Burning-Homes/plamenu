import assert from 'node:assert/strict';
import {test} from 'node:test';
import {mkdtempSync,mkdirSync,writeFileSync,symlinkSync,rmSync} from 'node:fs';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
import {observeOriginDirectory} from './origin-file-observer.mjs';

test('origin observation reads only the named owned UUID and exposes numeric metadata and hashes',()=>{
  const root=mkdtempSync(join(tmpdir(),'plamenu-origin-observer-')),uuid='00000000-0000-0000-0000-000000000001',dir=join(root,uuid);
  try {
    mkdirSync(dir);writeFileSync(join(dir,'secret.ts'),Buffer.alloc(16));
    writeFileSync(join(dir,'video.m3u8'),'#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:4,\nsecret.ts\n');
    symlinkSync('/unrelated/private',join(dir,'ignored.m3u8'));
    const rows=observeOriginDirectory(root,uuid);assert.equal(rows.length,1);assert.equal(rows[0].segmentCount,1);
    assert.equal(rows[0].latestBytes,16);assert.ok(rows[0].playlistMtimeMs>0);
    assert.ok(!JSON.stringify(rows).includes('secret'));assert.ok(!JSON.stringify(rows).includes(uuid));
    assert.throws(()=>observeOriginDirectory(root,'../unrelated'),/invalid owned UUID/);
  } finally {rmSync(root,{recursive:true});}
});
