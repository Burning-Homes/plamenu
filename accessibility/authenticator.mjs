import {createHmac} from 'node:crypto';

// Independent authenticator calculation from the setup key presented to the user.
export function totp(secret) {
  const alphabet='ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
  let bits=0,value=0;
  const bytes=[];
  for (const char of secret.trim().toUpperCase()) {
    value=(value<<5)|alphabet.indexOf(char);
    bits+=5;
    if (bits>=8) { bits-=8; bytes.push((value>>>bits)&255); }
  }
  const counter=Buffer.alloc(8);
  counter.writeBigUInt64BE(BigInt(Math.floor(Date.now()/30000)));
  const digest=createHmac('sha1',Buffer.from(bytes)).update(counter).digest();
  const offset=digest[digest.length-1]&15;
  return String((digest.readUInt32BE(offset)&0x7fffffff)%1000000).padStart(6,'0');
}

