import {readFileSync} from 'node:fs';
import {runInNewContext} from 'node:vm';
import {test} from 'node:test';
import assert from 'node:assert/strict';

// Execute the shipped choice function, not a second implementation. Browser
// journeys separately prove its real body attributes and pre-contact behavior.
const source=readFileSync(new URL('../../crates/server/src/web/assets/app.js',import.meta.url),'utf8');
const choice=source.slice(source.indexOf('  function peerSharingEnabled()'),source.indexOf('  function loadP2P('));
function enabled(dataset={},stored=null,current=null){
  return runInNewContext(`${choice};peerSharingEnabled()`,{p2pChoice:current,P2P_PREF:'plamenu:p2p-enabled',
    document:{body:{dataset},querySelector:()=>null},localStorage:{getItem:()=>stored}});
}
test('eligible new viewers default on and legacy browser opt-out remains off',()=>{
  assert.equal(enabled(),true);
  assert.equal(enabled({},'false'),false);
  assert.equal(enabled({p2pDefault:'true',p2pConfigured:'false'},'false'),false);
});
test('explicit account preferences outrank stale browser choices',()=>{
  assert.equal(enabled({p2pDefault:'false',p2pConfigured:'true'},'true'),false);
  assert.equal(enabled({p2pDefault:'true',p2pConfigured:'true'},'false'),true);
});
test('conservative migrated opt-out and failed preference read stay off',()=>{
  assert.equal(enabled({p2pDefault:'false',p2pConfigured:'false'},'true'),false);
  assert.equal(enabled({p2pDefault:'false',p2pConfigured:'true'}),false);
});
test('explicit server-only embed cannot be enabled by a playback choice',()=>{
  assert.equal(enabled({p2pDefault:'false',p2pLocked:'true'},'true',true),false);
});
