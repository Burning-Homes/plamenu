import {readFileSync} from 'node:fs';
import {runInNewContext} from 'node:vm';
import {createHash,webcrypto} from 'node:crypto';
import {test} from 'node:test';
import assert from 'node:assert/strict';

const source=readFileSync(new URL('../../crates/server/src/web/assets/app.js',import.meta.url),'utf8');
const start=source.indexOf('  function peerSegmentValidator(');
const end=source.indexOf('  async function peerOptions(',start);
assert(start>=0 && end>start,'extract the actual shipped segment validator');
const code=source.slice(start,end);
function validator(hashes){
  return runInNewContext(`${code};peerSegmentValidator(hashes,'https://local.test/media/hls/1/sha256.json?rev=1',false)`,
    {hashes,URL,TextDecoder,atob,crypto:webcrypto,location:{href:'https://local.test/@author/1'}});
}
const url='https://local.test/media/replacement.draft.hls480.m4s';
const range={start:32,end:43};
const newer=Buffer.from('NEWER-CONTNT!');
const older=Buffer.from('OLDER-CONTNT!');
const digest=bytes=>createHash('sha256').update(bytes).digest('hex');
const hashes={'replacement.draft.hls480.m4s':{'32-43':digest(newer)}};
test('new replacement bytes pass but equal-length old bytes cannot decode as replacement',async()=>{
  assert.equal(newer.byteLength,older.byteLength);
  const check=validator(hashes);
  assert.equal(await check(url,range,newer),true);
  assert.equal(await check(url,range,older),false);
});
test('old filename/checksum metadata cannot authorize replacement fragments',async()=>{
  const check=validator({'original.draft.hls480.m4s':{'32-43':digest(older)}});
  assert.equal(await check(url,range,older),false);
  assert.equal(await validator(hashes)(url,{start:33,end:44},newer),false);
});
test('missing or malformed integrity metadata fails closed before digest acceptance',async()=>{
  assert.equal(await validator({})(url,range,newer),false);
  assert.equal(await validator({'replacement.draft.hls480.m4s':{'32-43':'not-a-sha256'}})(url,range,newer),false);
});
