import assert from 'node:assert/strict';
import {test} from 'node:test';
import {readFileSync} from 'node:fs';
import {runInNewContext} from 'node:vm';

const source=readFileSync(new URL('../crates/server/src/web/assets/app.js',import.meta.url),'utf8');
const start=source.indexOf('const profileIssue =');
const end=source.indexOf('\n    if (profiling)',start);
assert.ok(start>=0&&end>start);
const observer=(profiling=true)=>runInNewContext(`
  const profileEvents=[];
  ${source.slice(start,end)}
  ({profileIssue,profileEvents});
`,{profiling,video:{currentTime:0,readyState:0},performance:{now:()=>42}});

test('existing opt-in HLS profile retains bounded enums without private error payloads',()=>{
  const {profileIssue,profileEvents}=observer();
  for(let n=0;n<200;n++)profileIssue('hls-media-error',false,{details:'bufferAppendError',
    error:{name:'InvalidStateError',message:'private URL'},frag:{level:0,sn:n},url:'private URL'});
  assert.equal(profileEvents.length,128);
  assert.equal(profileEvents[0].sequence,72);
  assert.equal(profileEvents.at(-1).details,'bufferAppendError');
  assert.equal(profileEvents.at(-1).exceptionName,'InvalidStateError');
  assert.ok(!JSON.stringify(profileEvents).includes('private'));
  profileIssue('hls-media-error',false,{details:'https://private',error:{name:'private URL'}});
  assert.equal(profileEvents.at(-1).details,null);
  assert.equal(profileEvents.at(-1).exceptionName,null);
});

test('disabled playback profiling performs no metadata reads',()=>{
  const {profileIssue,profileEvents}=observer(false);
  profileIssue('hls-media-error',false,new Proxy({}, {get(){throw Error('must not read');}}));
  assert.equal(profileEvents.length,0);
});
