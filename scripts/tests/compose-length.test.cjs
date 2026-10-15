const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');

// Exercise the shipped counter, not a test-side copy of its regular expression.
const source = fs.readFileSync(path.join(__dirname, '../../crates/server/src/web/assets/app.js'), 'utf8');
const start = source.indexOf('  const URL_PLACEHOLDER =');
const end = source.indexOf('  function setSectionOpen(', start);
assert.ok(start >= 0 && end > start);
const context = { Intl, TextEncoder, URL };
vm.runInNewContext(source.slice(start, end), context);
const count = context.weightedLength;

test('remote domains are free for Unicode, combining marks and bridge names', () => {
  for (const username of [
    'GrüneGera', 'Gru\u0308neGera', '你好', '用户', 'مستخدم', 'blue-sky.bsky.social',
    'npub1600yr4qg5vcfp7svf6ysj0008tn7aphnu0gjs6lw5hjn74n0laasjx889v',
  ]) {
    const expected = [...new Intl.Segmenter().segment(username)].length + 1;
    assert.equal(count('', `@${username}@remote.example`), expected, username);
    assert.equal(count('cw', `hi @${username}@remote.example.`), expected + 6, username);
  }
});

test('emails, malformed handles, oversized names and host suffixes stay whole', () => {
  for (const text of [
    'é@alice@remote.example', 'a\u0308@alice@remote.example', '_@alice@remote.example',
    '@@alice@remote.example', '/@alice@remote.example', '@bob@invalid', '@bob@123',
    '@bob.@remote.example', '@bob@remote.example_suffix', '@bob@remote.example@other',
    '@bob@xn--.example', '@\u0308alice@remote.example',
    `@${'a'.repeat(2049)}@remote.example`, `@bob@${'a'.repeat(64)}.example`,
  ]) {
    assert.equal(count('', text), [...new Intl.Segmenter().segment(text)].length, text);
  }
});

test('Unicode domains and mixed IDNA spellings have the same weight', () => {
  for (const domain of [
    'лемми.опенсорс.рус', 'лемми.опенсорс.xn--p1acf',
    'xn--e1aghfa.xn--e1aqbccjfc.xn--p1acf', 'bücher.example', 'bu\u0308cher.example',
    'bücher。example', 'remote.example用户', 'remote.example\u{10400}',
    'peertube.gröibschi.ch', 'peertube.xn--gribschi-o4a.ch',
  ]) {
    assert.equal(count('', `@Аим_Нискажуя@${domain}`), 13, domain);
    assert.equal(count('cw', `hi @Аим_Нискажуя@${domain}.`), 19, domain);
  }
});

test('local mentions, graphemes and URLs retain their weights', () => {
  assert.equal(count('', '@alice.'), 7);
  assert.equal(count('cw', 'hi @bob@remote.example'), 9);
  assert.equal(count('', '👩‍👩‍👧'), 1);
  assert.equal(count('', 'https://remote.example/@GrüneGera/123'), 23);
});
