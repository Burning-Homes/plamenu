// Opt-in passive diagnostics for the existing no-JS live media request.
// No interception, extra viewer, request, retry, or playback intervention.
import { closeSync, openSync, writeFileSync, writeSync } from 'node:fs';
import { join } from 'node:path';

export async function observeCaptureWire(context, page, evidence, kind) {
  const session = await context.newCDPSession(page);
  const requests = new Map(), pending = new Set();
  const report = {schemaVersion: 1, kind, observer: 'passive CDP resource stream', requests: [], unavailable: false};
  const maxBytes = 16 * 1024 * 1024, maxEvents = 20000;
  let stopped = false;
  function event(row, detail) {
    if (row.events.length < maxEvents) row.events.push(detail);
    else row.eventLimitReached = true;
  }
  function bytes(row, data, timestamp, buffered = false) {
    if (!data || row.fd === null) return;
    const chunk = Buffer.from(data, 'base64');
    if (row.bytes + chunk.length > maxBytes) {
      row.byteLimitReached = true;
      closeSync(row.fd); row.fd = null;
      return;
    }
    try {
      writeSync(row.fd, chunk);
      row.bytes += chunk.length;
      event(row, {event: 'bytes', timestamp, size: chunk.length, offset: row.bytes, buffered});
    } catch {
      row.writeUnavailable = true;
      try { closeSync(row.fd); } catch {}
      row.fd = null;
    }
  }
  session.on('Network.responseReceived', message => {
    let matches = false;
    try { matches = /^\/media\/live\/\d+\/stream\.mp4$/.test(new URL(message.response.url).pathname); } catch {}
    if (!matches || stopped) return;
    const file = `capture-${kind}-nojs-wire-${report.requests.length}.mp4`;
    const row = {file, status: message.response.status, protocol: message.response.protocol,
      timestamp: message.timestamp, bytes: 0, events: [], prefixReady: false, queue: [], fd: null};
    try { row.fd = openSync(join(evidence, file), 'wx', 0o600); }
    catch { row.writeUnavailable = true; }
    requests.set(message.requestId, row);
    report.requests.push(row);
    const task = session.send('Network.streamResourceContent', {requestId: message.requestId}).then(response => {
      bytes(row, response.bufferedData, message.timestamp, true);
      row.prefixReady = true;
      for (const item of row.queue) bytes(row, item.data, item.timestamp);
      row.queue = [];
      row.streamingAvailable = true;
    }).catch(() => {
      row.streamingAvailable = false; row.prefixReady = true; row.queue = [];
      report.unavailable = true;
    });
    pending.add(task);
    task.finally(() => pending.delete(task));
  });
  session.on('Network.dataReceived', message => {
    const row = requests.get(message.requestId);
    if (!row || stopped) return;
    if (!row.prefixReady) {
      if (row.queue.length < maxEvents) row.queue.push(message);
      else row.eventLimitReached = true;
    } else bytes(row, message.data, message.timestamp);
  });
  session.on('Network.loadingFinished', message => {
    const row = requests.get(message.requestId);
    if (row && !stopped) event(row, {event: 'finished', timestamp: message.timestamp});
  });
  session.on('Network.loadingFailed', message => {
    const row = requests.get(message.requestId);
    // Error text/URL/header/authentication are deliberately not retained.
    if (row && !stopped) event(row, {event: 'failed', timestamp: message.timestamp, canceled: !!message.canceled});
  });
  await session.send('Network.enable');
  return async () => {
    stopped = true;
    await Promise.allSettled([...pending]);
    await session.detach().catch(() => {});
    for (const row of report.requests) {
      if (row.fd !== null) { try { closeSync(row.fd); } catch {} }
      delete row.fd; delete row.queue; delete row.prefixReady;
    }
    writeFileSync(join(evidence, `capture-${kind}-nojs-wire.json`), JSON.stringify(report, null, 2), {mode: 0o600});
  };
}
