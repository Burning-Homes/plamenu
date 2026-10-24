import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import http from 'node:http';
import {spawn} from 'node:child_process';
import {fileURLToPath, pathToFileURL} from 'node:url';
import setup from './audio-setup.mjs';

const root = path.dirname(fileURLToPath(import.meta.url));

test('browser execution remains serial and every journey uses the owned fixture', async () => {
  await assert.rejects(setup({workers:2}), /workers=1/);
  for (const file of fs.readdirSync(path.join(root, 'tests'))) {
    if (!file.endsWith('.mjs')) continue;
    const source = fs.readFileSync(path.join(root, 'tests', file), 'utf8');
    assert.doesNotMatch(source, /from ['"]@playwright\/test['"]/, file);
    assert.match(source, /from ['"]\.\.\/fixtures\.mjs['"]/, file);
  }
});

test('real Playwright failure, retry and next test each release their own fixture', async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'plamenu-fixture-contract-'));
  const events = [];
  const server = http.createServer((request, response) => {
    let body = '';
    request.on('data', chunk => { body += chunk; });
    request.on('end', () => {
      events.push([request.url, JSON.parse(body).case, JSON.parse(body).cold_rendition]);
      response.writeHead(204); response.end();
    });
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  try {
    fs.writeFileSync(path.join(directory, 'playwright.config.mjs'), `export default {testDir:'.', outputDir:${JSON.stringify(path.join(directory, 'results'))}, workers:1, retries:1, reporter:'line'};`);
    fs.writeFileSync(path.join(directory, 'lifecycle.spec.mjs'), `
      import {test, expect} from ${JSON.stringify(pathToFileURL(path.join(root, 'fixtures.mjs')).href)};
      test('fails once', async ({}, info) => { expect(info.retry).toBe(1); });
      test('fresh next case', async () => {});
      test.describe('cold only', () => {
        test.use({coldRendition:true});
        test('releases only its own encoder', async ({coldEncoding}) => { await coldEncoding.release(); });
      });
    `);
    const child = spawn(process.execPath, [path.join(root, 'node_modules/@playwright/test/cli.js'), 'test', '--config', path.join(directory, 'playwright.config.mjs')], {
      env: {...process.env, A11Y_FIXTURE_CONTROL_URL:`http://127.0.0.1:${server.address().port}`}, stdio:['ignore','pipe','pipe'],
    });
    let output = '';
    child.stdout.on('data', data=>{ output += data; }); child.stderr.on('data', data=>{ output += data; });
    const status = await new Promise((resolve,reject)=>{ child.once('error',reject); child.once('exit',resolve); });
    assert.equal(status, 0, output);
    assert.equal(events.length, 9, output);
    assert.deepEqual(events.map(event=>event[0]), ['/begin','/end','/begin','/end','/begin','/end','/begin','/release-encoder','/end']);
    assert.deepEqual(events.map(event=>event[2]), [false,false,false,false,false,false,true,true,true]);
    assert.equal(events[6][1], events[7][1]);
    const lifecycle = events.filter(event=>event[0] !== '/release-encoder');
    for (let index = 0; index < lifecycle.length; index += 2) assert.equal(lifecycle[index][1], lifecycle[index+1][1]);
    assert.equal(new Set(lifecycle.filter((_,index)=>index%2===0).map(event=>event[1])).size, 4);
  } finally {
    await new Promise(resolve=>server.close(resolve));
    fs.rmSync(directory, {recursive:true,force:true});
  }
});

test('missing audio executable fails promptly and removes its temporary directory', async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'plamenu-audio-contract-'));
  try {
    const program = `import assert from 'node:assert/strict';
      import setup from ${JSON.stringify(pathToFileURL(path.join(root, 'audio-setup.mjs')).href)};
      await assert.rejects(setup({workers:1}), /audio backend failed/);`;
    const child = spawn(process.execPath, ['--input-type=module', '-e', program], {
      env: {...process.env, A11Y_NULL_AUDIO:'1', PATH:directory, TMPDIR:directory}, stdio:['ignore','pipe','pipe'],
    });
    let output = '';
    child.stderr.on('data', data=>{ output += data; });
    const timeout = setTimeout(()=>child.kill('SIGKILL'), 10_000);
    try {
      const status = await new Promise((resolve,reject)=>{ child.once('error',reject); child.once('exit',resolve); });
      assert.equal(status, 0, output);
      assert.deepEqual(fs.readdirSync(directory), []);
    } finally { clearTimeout(timeout); }
  } finally { fs.rmSync(directory, {recursive:true,force:true}); }
});
