// Test-only malicious seed. The shipped receiving player and its checksum
// validator are unchanged. Framed p2p-media-loader 4 commands are never edited;
// only actual outbound raw media chunks can be corrupted or replayed stale.
export function installPeerWireFault() {
  const realSend = RTCDataChannel.prototype.send;
  const retained = new Map();
  const observation = window.__ownedPeerWireFault = {mode:'healthy',rawBytes:0,changedBytes:0,
    changedChunks:0,commands:0,retainedBytes:0,skippedIdentical:0};
  const token = (bytes, at) => String.fromCharCode(...bytes.subarray(at, at+4));
  RTCDataChannel.prototype.send = function(payload) {
    const bytes = payload instanceof ArrayBuffer ? new Uint8Array(payload) :
      ArrayBuffer.isView(payload) ? new Uint8Array(payload.buffer,payload.byteOffset,payload.byteLength) : null;
    if(!bytes || bytes.byteLength<8) return realSend.call(this,payload);
    const head=token(bytes,0),tail=token(bytes,bytes.byteLength-4);
    if(['cstr','dstr'].includes(head)&&['cend','dend'].includes(tail)) {
      observation.commands++;return realSend.call(this,payload);
    }
    // Peer loader's own maximum message is bounded. Never retain arbitrary
    // browser buffers or edit a tiny control/other-product payload.
    if(bytes.byteLength<1024||bytes.byteLength>65536)return realSend.call(this,payload);
    observation.rawBytes+=bytes.byteLength;
    const sameLength=retained.get(bytes.byteLength);
    const previous=sameLength||[...retained.values()].find(chunk=>chunk.byteLength>=bytes.byteLength);
    let outgoing=bytes;
    if(observation.mode==='corrupt') {
      outgoing=bytes.slice();outgoing[Math.floor(outgoing.length/2)]^=1;
    } else if(observation.mode==='stale'&&previous) {
      const older=previous.subarray(0,bytes.byteLength);
      if(older.some((byte,n)=>byte!==bytes[n]))outgoing=older;
      else observation.skippedIdentical++;
    }
    if(outgoing!==bytes){observation.changedChunks++;observation.changedBytes+=outgoing.byteLength;}
    const copy=bytes.slice();
    if(sameLength)observation.retainedBytes-=sameLength.byteLength;
    retained.delete(copy.byteLength);retained.set(copy.byteLength,copy);
    observation.retainedBytes+=copy.byteLength;
    while(retained.size>16||observation.retainedBytes>1024*1024){
      const key=retained.keys().next().value;observation.retainedBytes-=retained.get(key).byteLength;retained.delete(key);
    }
    return realSend.call(this,outgoing);
  };
}
