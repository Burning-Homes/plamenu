import fs from 'node:fs';
import path from 'node:path';
import {createHash, randomUUID} from 'node:crypto';
import {fileURLToPath} from 'node:url';

export const runDirectory = process.env.A11Y_RUN_DIR || path.resolve(
  path.dirname(fileURLToPath(import.meta.url)), '../target/accessibility/runs', randomUUID(),
);
export const digest = value => createHash('sha256').update(value).digest('hex');
export function stable(value) {
  if (Array.isArray(value)) return `[${value.map(stable).join(',')}]`;
  if (value && typeof value === 'object') return `{${Object.keys(value).sort()
    .map(key => `${JSON.stringify(key)}:${stable(value[key])}`).join(',')}}`;
  return JSON.stringify(value);
}

export function loadInventory(root) {
  const inventory = JSON.parse(fs.readFileSync(path.join(root, 'accessibility/inventory.json')));
  const files = {};
  function walk(directory) {
    for (const entry of fs.readdirSync(directory, {withFileTypes:true})) {
      const file = path.join(directory, entry.name);
      if (entry.isDirectory()) walk(file);
      else if (entry.name.endsWith('.mjs')) files[path.relative(root, file)] = digest(fs.readFileSync(file));
    }
  }
  walk(path.join(root, 'accessibility/tests'));
  const helpers = fs.readdirSync(path.join(root, 'accessibility')).filter(name=>name.endsWith('.mjs') && !name.endsWith('.test.mjs'));
  for (const name of [...helpers, 'package.json', 'package-lock.json', 'coverage-policy.json']) {
    const file = `accessibility/${name}`;
    files[file] = digest(fs.readFileSync(path.join(root, file)));
  }
  for (const file of ['scripts/run-accessibility-tests.py', 'scripts/browser_fixture.py',
    'scripts/cold_rendition_fixture.py',
    'scripts/accessibility-container.sh', 'e2e/plamenu_e2e/lifecycle.py',
    ...fs.readdirSync(path.join(root, 'accessibility/fixtures')).filter(name=>name.endsWith('.sql')).map(name=>`accessibility/fixtures/${name}`)]) {
    files[file] = digest(fs.readFileSync(path.join(root, file)));
  }
  if (inventory.schema !== 1 || inventory.suite !== 'browser' || stable(files) !== stable(inventory.files)) {
    throw new Error('Stale browser inventory; run ./dev gate-inventory --write and review the diff');
  }
  if (!inventory.tests.length || new Set(inventory.tests.map(t=>t.id)).size !== inventory.tests.length) {
    throw new Error('Empty or duplicate browser inventory');
  }
  return inventory;
}

export function allowedSkip(test, policy) {
  return test.annotations?.length === 1 && test.annotations[0].type === 'skip'
    && policy.exclusions.some(rule => rule.file === test.file && rule.title === test.title
      && rule.projects.includes(test.project) && rule.reason === test.annotations[0].description);
}

export function assess(inventory, selected, tests, policy) {
  const expected = inventory.tests.map(t=>t.id);
  const same = (a,b) => a.length === b.length && new Set(a).size === a.length
    && stable([...a].sort()) === stable([...b].sort());
  const complete = same(expected, selected) && same(expected, tests.map(t=>t.id));
  const exclusions = [], failures = [];
  const cases = new Map(inventory.tests.map(t=>[t.id,t]));
  for (const test of tests) {
    const declared = cases.get(test.id);
    if (!declared || ['file','title','project'].some(key=>test[key] !== declared[key])) {
      failures.push(test.id); continue;
    }
    const attempts = test.attempts;
    if (!attempts?.length || attempts.length !== 1 || attempts[0].retry !== 0) {
      failures.push(test.id); continue;
    }
    const result = attempts[0];
    if (result.status === 'passed' && test.expected_status === 'passed' && test.annotations?.length === 0) continue;
    if (result.status === 'skipped' && allowedSkip({...cases.get(test.id), annotations:test.annotations}, policy)) {
      exclusions.push(test.id);
    } else failures.push(test.id);
  }
  return {complete, exclusions, failures};
}
