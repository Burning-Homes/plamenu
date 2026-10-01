import {test} from 'node:test';
import assert from 'node:assert/strict';
import {installPeerWireFault} from './p2p-wire-fault.mjs';

test('wire fault leaves framed commands exact and mutates only bounded copied media views',()=>{
  const priorWindow=globalThis.window,priorChannel=globalThis.RTCDataChannel,sent=[];
  globalThis.window={};
  globalThis.RTCDataChannel=class{send(data){sent.push(typeof data==='string'?data:new Uint8Array(data.buffer??data,data.byteOffset??0,data.byteLength).slice());}};
  try{
    installPeerWireFault();const channel=new RTCDataChannel(),control=new Uint8Array(16384);
    control.set(new TextEncoder().encode('cstr'));control.set(new TextEncoder().encode('cend'),control.length-4);
    window.__ownedPeerWireFault.mode='corrupt';channel.send(control);
    assert.deepEqual(sent.at(-1),control);assert.equal(window.__ownedPeerWireFault.commands,1);
    const container=new Uint8Array(2048).fill(7),view=container.subarray(128,1152);
    channel.send(view);assert.equal(sent.at(-1).length,1024);assert.equal(sent.at(-1)[512],6);
    assert.ok(container.every(byte=>byte===7),'source buffers must not be changed');
    window.__ownedPeerWireFault.mode='stale';channel.send(new Uint8Array(1024).fill(9));
    assert.ok(sent.at(-1).every(byte=>byte===7),'send the genuinely retained older chunk');
    for(let n=1024;n<1060;n++)channel.send(new Uint8Array(n).fill(n));
    assert.ok(window.__ownedPeerWireFault.retainedBytes<=1024*1024);
    const changed=window.__ownedPeerWireFault.changedBytes;
    channel.send(new Uint8Array(65537));channel.send('command');
    assert.equal(window.__ownedPeerWireFault.changedBytes,changed);
    assert.equal(sent.at(-1),'command');
  }finally{globalThis.window=priorWindow;globalThis.RTCDataChannel=priorChannel;}
});
