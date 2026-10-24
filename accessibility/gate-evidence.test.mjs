import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {execFileSync, spawnSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';
import {assess, loadInventory} from './gate-evidence.mjs';
import EvidenceReporter, {sourceIdentity} from './evidence-reporter.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const policy = JSON.parse(fs.readFileSync(path.join(root, 'accessibility/coverage-policy.json')));
const inventory = loadInventory(root);
const passed = item => ({...item, expected_status:'passed', annotations:[], attempts:[{retry:0, status:'passed'}]});

test('source identity handles large real Git patches and hashes changes beyond the old buffer', () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'plamenu-large-patch-'));
  const git = args => execFileSync('git', args, {cwd:directory, stdio:'pipe'});
  try {
    git(['init', '-q']);
    const file = path.join(directory, 'large.txt');
    const prefix = 'a'.repeat(1_100_000);
    fs.writeFileSync(file, prefix + '\n');
    git(['add', 'large.txt']);
    // This disposable Git fixture has no relation to the maintained repository.
    git(['-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.test',
      '-c', 'commit.gpgsign=false', '-c', 'core.hooksPath=/dev/null', 'commit', '-qm', 'fixture']);
    fs.writeFileSync(file, prefix + 'b\n');
    const limited = spawnSync('git', ['diff', '--binary', 'HEAD'], {cwd:directory});
    assert.equal(limited.error?.code, 'ENOBUFS');
    const first = sourceIdentity(directory);
    assert.equal(first.source_dirty, true);
    fs.writeFileSync(file, prefix + 'c\n');
    const second = sourceIdentity(directory);
    assert.equal(first.source_revision, second.source_revision);
    assert.notEqual(first.source_patch_sha256, second.source_patch_sha256);
  } finally { fs.rmSync(directory, {recursive:true, force:true}); }
});

test('browser and Python consumers bind the same fixture sources', () => {
  const files = JSON.parse(execFileSync('python3', ['-c',
    'import json,sys; sys.path.insert(0,"scripts"); from gate_inventory import ROOT,source_files; print(json.dumps(source_files(ROOT,"browser")))',
  ], {cwd:root, encoding:'utf8'}));
  assert.deepEqual(inventory.files, files);
  assert.ok(files['scripts/cold_rendition_fixture.py']);
});

test('full, partial, interrupted, repeated and retrying execution stay distinct', () => {
  const ids = inventory.tests.map(t=>t.id), results = inventory.tests.map(passed);
  assert.deepEqual(assess(inventory, ids, results, policy), {complete:true, exclusions:[], failures:[]});
  assert.equal(assess(inventory, ids.slice(1), results.slice(1), policy).complete, false);
  assert.equal(assess(inventory, ids, results.slice(1), policy).complete, false);
  assert.equal(assess(inventory, [...ids,ids[0]], results, policy).complete, false);
  results[0].attempts = [{retry:0,status:'failed'},{retry:1,status:'passed'}];
  assert.deepEqual(assess(inventory, ids, results, policy).failures, [ids[0]]);
});

test('known defects and unknown skips cannot borrow an exclusion reason', () => {
  const item = inventory.tests.find(t=>t.file === 'client-navigation-review.spec.mjs' && t.project === 'chromium-mobile');
  const result = passed(item);
  result.attempts[0].status = 'skipped'; result.expected_status = 'skipped';
  result.annotations = [{type:'skip',description:policy.exclusions[0].reason}];
  assert.deepEqual(assess(inventory, [item.id], [result], policy).exclusions, [item.id]);
  result.annotations[0].type = 'fixme';
  assert.deepEqual(assess(inventory, [item.id], [result], policy).failures, [item.id]);
  result.annotations = [{type:'skip',description:'fixture unavailable'}];
  assert.deepEqual(assess(inventory, [item.id], [result], policy).failures, [item.id]);
});

test('real Playwright collection is never execution evidence, and existing reports survive', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'plamenu-gate-'));
  try {
    const output = path.join(dir, 'report.json');
    const args = ['accessibility/node_modules/@playwright/test/cli.js','test','--config','accessibility/playwright.config.mjs','--list'];
    const env = {...process.env, A11Y_REPORT_PATH:output};
    execFileSync(process.execPath, args, {cwd:root, env, stdio:'pipe'});
    const bytes = fs.readFileSync(output);
    const report = JSON.parse(bytes);
    assert.equal(report.selected.length, inventory.tests.length);
    assert.equal(report.full_pass, false);
    assert.deepEqual(report.tests, []);
    const retry = spawnSync(process.execPath, args, {cwd:root, env});
    assert.notEqual(retry.status, 0);
    assert.deepEqual(fs.readFileSync(output), bytes);
  } finally { fs.rmSync(dir, {recursive:true,force:true}); }
});

test('reporter keeps failed attempts and never promotes a passing subset', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'plamenu-reporter-'));
  const old = process.env.A11Y_REPORT_PATH;
  try {
    process.env.A11Y_REPORT_PATH = path.join(dir, 'report.json');
    const item = inventory.tests[0];
    const testcase = {id:item.id, expectedStatus:'passed', annotations:[],
      parent:{project:()=>({name:item.project})},
      titlePath:()=>['',item.project,item.file,item.title],
      location:{file:path.join(root,'accessibility/tests',item.file)}};
    const reporter = new EvidenceReporter();
    reporter.onBegin({projects:[{name:item.project}]}, {allTests:()=>[testcase]});
    reporter.onTestEnd(testcase, {retry:0,status:'failed',duration:1});
    reporter.onTestEnd(testcase, {retry:1,status:'passed',duration:1});
    reporter.onEnd({status:'passed'});
    const report = JSON.parse(fs.readFileSync(process.env.A11Y_REPORT_PATH));
    assert.equal(report.full_pass, false);
    assert.equal(report.tests[0].attempts.length, 2);
    assert.deepEqual(report.failures, [item.id]);
  } finally {
    if (old === undefined) delete process.env.A11Y_REPORT_PATH; else process.env.A11Y_REPORT_PATH = old;
    fs.rmSync(dir, {recursive:true,force:true});
  }
});

test('Python consumer accepts complete reporter evidence and rejects a forged partial pass', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'plamenu-evidence-contract-'));
  const old = process.env.A11Y_REPORT_PATH;
  const oldControl = process.env.A11Y_FIXTURE_CONTROL_URL;
  try {
    const output = path.join(dir, 'synthetic-report.json');
    process.env.A11Y_REPORT_PATH = output;
    process.env.A11Y_FIXTURE_CONTROL_URL = 'http://owned-fixture.invalid';
    const cases = inventory.tests.map(item=>({id:item.id, expectedStatus:'passed', annotations:[],
      parent:{project:()=>({name:item.project})}, titlePath:()=>['',item.project,item.file,item.title],
      location:{file:path.join(root,'accessibility/tests',item.file)}}));
    const reporter = new EvidenceReporter();
    reporter.onBegin({projects:[...new Set(inventory.tests.map(t=>t.project))].map(name=>({name}))}, {allTests:()=>cases});
    for (const item of cases) reporter.onTestEnd(item, {retry:0,status:'passed',duration:1});
    reporter.onEnd({status:'passed'});
    const args = ['-c', 'import runpy,sys; from pathlib import Path; m=runpy.run_path(sys.argv[1]); m["validate_automated"](Path(sys.argv[2]))',
      path.join(root,'scripts/accessibility-evidence.py'), output];
    // This is synthetic reporter input, deliberately outside any release-evidence directory.
    assert.equal(JSON.parse(fs.readFileSync(output)).full_pass, false);
    assert.notEqual(spawnSync('python3', args, {cwd:root}).status, 0);
    execFileSync('python3', ['-c', `import json,sys; from pathlib import Path; sys.path.insert(0,'scripts'); from browser_fixture import finalize_report
p=Path(sys.argv[1]); r=json.loads(p.read_text()); events=[{'case': t['id']+':0:nonce','action': a,'passed': True} for t in r['tests'] for a in ('begin','end')]; finalize_report(p,events,0)`, output], {cwd:root, stdio:'pipe'});
    execFileSync('python3', args, {cwd:root, stdio:'pipe'});
    const record = JSON.parse(fs.readFileSync(output));
    assert.equal(record.full_pass, true);
    record.tests.pop();
    fs.writeFileSync(output, JSON.stringify(record));
    const rejected = spawnSync('python3', args, {cwd:root, encoding:'utf8'});
    assert.notEqual(rejected.status, 0);
    assert.match(rejected.stderr, /partial browser selection or execution/);
  } finally {
    if (oldControl === undefined) delete process.env.A11Y_FIXTURE_CONTROL_URL; else process.env.A11Y_FIXTURE_CONTROL_URL = oldControl;
    if (old === undefined) delete process.env.A11Y_REPORT_PATH; else process.env.A11Y_REPORT_PATH = old;
    fs.rmSync(dir, {recursive:true,force:true});
  }
});
