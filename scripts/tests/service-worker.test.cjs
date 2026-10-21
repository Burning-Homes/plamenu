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
    self: { location: { origin: 'https://plamenu.local' }, registration: { showNotification: async (title, options) => { calls.push(['notification', title, options]); } }, clients: { matchAll: async () => [], openWindow: async url => { calls.push(['open', url]); } }, addEventListener: (name, handler) => handlers.set(name, handler) },
    caches: { open: async (name) => {
      calls.push(['cache', name]);
      return { match: async (key) => { calls.push(['match', key]); return 'cached-current'; } };
    } },
    fetch: async (request, options) => { calls.push(['fetch', request.url, options]); return 'network'; },
  };
  vm.runInNewContext(source, context);
  return { calls, handlers, context, fetch: (url) => {
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

async function push(w, payload) {
  let pending;
  w.handlers.get('push')({ data: { json: () => payload }, waitUntil: work => { pending = work; } });
  await pending;
  return w.calls.find(call => call[0] === 'notification')[2];
}

async function click(w, data) {
  let pending;
  w.handlers.get('notificationclick')({ notification: { data, close() {} }, waitUntil: work => { pending = work; } });
  await pending;
}

test('private mention and reaction pushes open their exact post with its fragment', async () => {
  for (const type of ['mention', 'favourite', 'pleroma:emoji_reaction']) {
    const w = worker();
    const url = '/web/statuses/117444945448468480#post-117444945448468480';
    const options = await push(w, { notification_type: type, url });
    assert.equal(options.data.url, url);
    await click(w, options.data);
    assert.deepEqual(w.calls.at(-1), ['open', url]);
  }
});

test('old, malformed and external push targets safely open the inbox', async () => {
  for (const url of [undefined, 'https://other.test/private', 'javascript:alert(1)', 'http://[']) {
    const w = worker();
    const options = await push(w, { url });
    await click(w, options.data);
    assert.deepEqual(w.calls.at(-1), ['open', '/notifications']);
  }
});

test('an existing target refreshes and focuses without replacing a draft tab', async () => {
  const w = worker();
  w.context.self.clients.matchAll = async () => [
    { url: 'https://plamenu.local/compose', navigate() { assert.fail('draft navigation'); } },
    { url: 'https://plamenu.local/notifications', navigate: async url => w.calls.push(['navigate', url]), focus: async () => w.calls.push(['focus']) },
  ];
  await click(w, { url: '/notifications' });
  assert.deepEqual(w.calls, [['navigate', '/notifications'], ['focus']]);
});
