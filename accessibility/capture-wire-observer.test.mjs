import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { mkdtempSync, readFileSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { observeCaptureWire } from './capture-wire-observer.mjs';

test('passive capture preserves buffered prefix before concurrent live chunks', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'plamenu-wire-'));
  try {
    const session = new EventEmitter(), commands = [];
    let complete;
    session.send = (method, params) => {
      commands.push({method, params});
      if (method === 'Network.streamResourceContent') return new Promise(resolve => { complete = resolve; });
      return Promise.resolve({});
    };
    session.detach = () => Promise.resolve();
    const stop = await observeCaptureWire({newCDPSession: async () => session}, {}, dir, 'camera');
    session.emit('Network.responseReceived', {requestId: 'other', timestamp: 0,
      response: {url: 'https://example.test/private?token=never-retain', status: 200, protocol: 'h2'}});
    session.emit('Network.responseReceived', {requestId: 'owned', timestamp: 1,
      response: {url: 'https://example.test/media/live/123/stream.mp4?token=never-retain', status: 200, protocol: 'h2'}});
    const data = (value, timestamp) => session.emit('Network.dataReceived', {
      requestId: 'owned', timestamp, data: Buffer.from(value).toString('base64'),
    });
    data('second', 2);
    complete({bufferedData: Buffer.from('first').toString('base64')});
    await new Promise(resolve => setImmediate(resolve));
    data('third', 3);
    await stop();
    assert.equal(readFileSync(join(dir, 'capture-camera-nojs-wire-0.mp4'), 'utf8'), 'firstsecondthird');
    const text = readFileSync(join(dir, 'capture-camera-nojs-wire.json'), 'utf8'), report = JSON.parse(text);
    assert.equal(text.includes('never-retain'), false);
    assert.equal(report.requests.length, 1);
    assert.deepEqual(report.requests[0].events.map(event => event.offset), [5, 11, 16]);
    assert.equal(report.requests[0].streamingAvailable, true);
    assert.equal(statSync(join(dir, 'capture-camera-nojs-wire-0.mp4')).mode & 0o777, 0o600);
    assert.deepEqual(commands.map(command => command.method), ['Network.enable', 'Network.streamResourceContent']);
  } finally { rmSync(dir, {recursive: true}); }
});

test('unsupported passive resource streaming is recorded without a playback action', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'plamenu-wire-'));
  try {
    const session = new EventEmitter();
    session.send = method => method === 'Network.enable' ? Promise.resolve({}) : Promise.reject(new Error('unsupported'));
    session.detach = () => Promise.resolve();
    const stop = await observeCaptureWire({newCDPSession: async () => session}, {}, dir, 'screen');
    session.emit('Network.responseReceived', {requestId: 'owned', timestamp: 1,
      response: {url: 'https://example.test/media/live/123/stream.mp4', status: 200, protocol: 'h2'}});
    await stop();
    const report = JSON.parse(readFileSync(join(dir, 'capture-screen-nojs-wire.json'), 'utf8'));
    assert.equal(report.unavailable, true);
    assert.equal(report.requests[0].streamingAvailable, false);
  } finally { rmSync(dir, {recursive: true}); }
});
