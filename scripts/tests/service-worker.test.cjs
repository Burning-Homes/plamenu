const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');
const source = fs.readFileSync(path.join(__dirname, '../../crates/server/src/web/assets/sw.js'), 'utf8');

function worker() {
  const handlers = new Map();
  const calls = [];
  const context = {
    PLAMENU_ASSET_VERSION: 'current', URL,
    self: { location: { origin: 'https://plamenu.local' }, addEventListener: (name, handler) => handlers.set(name, handler) },
    caches: { open: async (name) => {
      calls.push(['cache', name]);
      return { match: async (key) => { calls.push(['match', key]); return 'cached-current'; } };
    } },
    fetch: async (request, options) => { calls.push(['fetch', request.url, options]); return 'network'; },
  };
  vm.runInNewContext(source, context);
  return { calls, fetch: (url) => {
    let response;
    handlers.get('fetch')({ request: { method: 'GET', mode: 'cors', url }, respondWith: (value) => { response = value; } });
    return response;
  } };
}

test('new deployment asset URLs never receive the previous worker cache', async () => {
  const w = worker();
  assert.equal(await w.fetch('https://plamenu.local/assets/app.js?v=next'), 'network');
  assert.equal(w.calls.length, 1);
  assert.equal(w.calls[0][0], 'fetch');
  assert.equal(w.calls[0][2].cache, 'reload');
});

test('matching versions read only their own named cache', async () => {
  const w = worker();
  assert.equal(await w.fetch('https://plamenu.local/assets/app.css?v=current'), 'cached-current');
  assert.deepEqual(w.calls, [['cache', 'plamenu-shell-current'], ['match', '/assets/app.css']]);
});

test('personalized API responses are not intercepted', () => {
  const w = worker();
  assert.equal(w.fetch('https://plamenu.local/api/v1/timelines/home'), undefined);
  assert.deepEqual(w.calls, []);
});
