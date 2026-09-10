// Run with: node --test crates/server/tests/webxdc_bridge.mjs
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

function bridge() {
  const events = new Map();
  const sent = [];
  const fetches = [];
  const parent = { postMessage() {} };
  const port = { postMessage(message) { sent.push(message); }, start() {} };
  const window = {
    __plamenuWebxdc: { session: "9007199254741001", hostOrigin: "https://plamenu.test",
      selfAddr: "pseudonym", selfName: "Name", sendUpdateInterval: 0, sendUpdateMaxSize: 128000 },
    parent, location: { href: "https://app.test/index.html", protocol: "https:", host: "app.test" },
    fetch(input, init) { fetches.push({ input, init }); return Promise.resolve("asset"); },
    addEventListener(type, callback) { events.set(type, callback); },
    removeEventListener(type) { events.delete(type); },
  };
  vm.runInNewContext(readFileSync(new URL("../src/web/assets/webxdc-bridge.js", import.meta.url), "utf8"),
    { window, crypto: { randomUUID() { return "runtime-1"; } }, navigator: {}, Request, URL, Uint8Array, setInterval() { return 1; }, clearInterval() {} });
  const connect = () => events.get("message")({ origin: "https://plamenu.test", source: parent,
    data: { channel: "plamenu-webxdc-init", session: "9007199254741001", runtimeId: "runtime-1" }, ports: [port] });
  return { api: window.webxdc, fetch: window.fetch, fetches, sent, connect, receive: (generation, bytes) => port.onmessage({
    data: { channel: "plamenu-webxdc", session: "9007199254741001", op: "realtime", generation, value: bytes },
  }) };
}

test("asset fetch supports local textures while rejecting other origins and schemes", async () => {
  const host = bridge();
  for (const input of ["textures/fish.png", "/model.glb", "https://app.test/texture.png",
    "blob:https://app.test/texture", "data:image/png;base64,AA==",
    new Request("blob:https://app.test/bitmap")]) {
    assert.equal(await host.fetch(input, { credentials: "include", redirect: "follow" }), "asset");
    const call = host.fetches.at(-1);
    assert.equal(call.input, input);
    assert.equal(call.init.credentials, "omit");
    assert.equal(call.init.redirect, "error");
  }
  const allowed = host.fetches.length;
  for (const input of ["https://plamenu.test/health", "//external.test/texture.png",
    "http://app.test/texture.png", "https://app.test:444/texture.png",
    "blob:https://external.test/texture", "file:///texture.png", "javascript:alert(1)",
    new Request("https://external.test/texture.png")]) {
    await assert.rejects(host.fetch(input), /Internet access is disabled/);
  }
  assert.equal(host.fetches.length, allowed, "blocked requests never reach native fetch");
});

test("realtime API validates data and never replays across leave/rejoin", async () => {
  const host = bridge();
  const first = host.api.joinRealtimeChannel();
  assert.throws(() => host.api.joinRealtimeChannel(), /already joined/);
  assert.throws(() => first.send([1]), /Uint8Array/);
  assert.throws(() => first.send(new Uint8Array(128001)), /128000/);
  first.send(new Uint8Array([99])); // no MessagePort: discard
  host.connect();
  await Promise.resolve();
  assert.deepEqual(host.sent.map(m => m.op), ["realtimeJoin"]);
  first.send(new Uint8Array(128000));
  assert.equal(host.sent.at(-1).value.byteLength, 128000);
  const received = [];
  host.receive(1, new Uint8Array([0])); // no listener: discard
  first.setListener(bytes => received.push([...bytes]));
  host.receive(1, new Uint8Array([0, 255]));
  first.setListener(bytes => received.push([42, ...bytes]));
  host.receive(1, new Uint8Array([1]));
  first.leave();
  assert.throws(() => first.send(new Uint8Array()), /left/);
  assert.throws(() => first.setListener(() => {}), /left/);
  const second = host.api.joinRealtimeChannel();
  second.setListener(bytes => received.push([...bytes]));
  await Promise.resolve();
  host.receive(1, new Uint8Array([99])); // delayed old channel event
  host.receive(2, new Uint8Array([2]));
  assert.deepEqual(received, [[0, 255], [42, 1], [2]]);
  second.leave();
});

test("leaving before bridge bootstrap does not join later", async () => {
  const host = bridge();
  host.api.joinRealtimeChannel().leave();
  host.connect();
  await Promise.resolve();
  assert.deepEqual(host.sent, []);
});

test("trusted host drops disconnected/busy sends and isolates reloaded runtimes", async () => {
  const events = new Map();
  const sockets = [];
  const channels = [];
  const frame = { dataset: { webxdcSession: "9007199254741001", webxdcOrigin: "https://app.test" },
    contentWindow: { postMessage() {} }, remove() { this.removed = true; } };
  class Socket {
    static OPEN = 1;
    constructor() { this.readyState = 0; this.bufferedAmount = 0; this.sent = []; sockets.push(this); }
    send(value) { this.sent.push(value); }
    close() { this.closed = true; this.onclose?.({ code: 1000 }); }
  }
  class Channel {
    constructor() {
      this.port1 = { sent: [], postMessage(message) { this.sent.push(message); },
        start() {}, close() { this.closed = true; } };
      this.port2 = {};
      channels.push(this);
    }
  }
  vm.runInNewContext(readFileSync(new URL("../src/web/assets/webxdc-host.js", import.meta.url), "utf8"), {
    document: { querySelector(selector) { return selector.startsWith("iframe") ? frame : null; } },
    window: { addEventListener(type, callback) { events.set(type, callback); } },
    location: { host: "plamenu.test" }, WebSocket: Socket, MessageChannel: Channel, Uint8Array, ArrayBuffer,
    setInterval() {}, clearTimeout() {}, setTimeout() { throw new Error("unexpected reconnect"); },
  });
  const ready = runtimeId => events.get("message")({ source: frame.contentWindow, origin: "https://app.test",
    data: { channel: "plamenu-webxdc-ready", session: frame.dataset.webxdcSession, runtimeId } });
  const send = (op, generation, value) => channels.at(-1).port1.onmessage({ data: {
    channel: "plamenu-webxdc", session: frame.dataset.webxdcSession, op, generation, value,
  } });
  ready("runtime-1");
  ready("runtime-1");
  assert.equal(channels.length, 1);
  await send("realtimeJoin", 1);
  await send("realtimeSend", 1, new Uint8Array([1]));
  assert.equal(sockets[0].sent.length, 0);
  sockets[0].readyState = Socket.OPEN;
  await send("realtimeSend", 1, new Uint8Array([2]));
  sockets[0].bufferedAmount = 1;
  await send("realtimeSend", 1, new Uint8Array([3]));
  assert.deepEqual(sockets[0].sent.map(bytes => [...bytes]), [[2]]);
  await send("realtimeLeave", 1);
  assert.equal(frame.removed, undefined, "leaving a channel must not close the app");
  await send("realtimeJoin", 2);
  sockets[0].onmessage({ data: new Uint8Array([99]).buffer });
  assert.equal(channels[0].port1.sent.length, 0);
  sockets[1].onmessage({ data: new Uint8Array([4]).buffer });
  assert.deepEqual([...channels[0].port1.sent[0].value], [4]);
  ready("runtime-2");
  assert.equal(sockets[1].closed, true);
  assert.equal(channels[0].port1.closed, true);
  assert.equal(channels.length, 2);
  sockets[1].onmessage({ data: new Uint8Array([99]).buffer });
  assert.equal(channels[1].port1.sent.length, 0);
});
