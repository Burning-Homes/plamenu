import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';
import {assess, digest, loadInventory, runDirectory, stable} from './gate-evidence.mjs';

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
export function sourceIdentity(root = repository) {
  const git = args => execFileSync('git', args, {cwd:root, encoding:'utf8'}).trim();
  const status = git(['status', '--porcelain']);
  const revision = git(['rev-parse', 'HEAD']);
  // Large refactors exceed Node's default captured-stdout limit. Git writes
  // the complete patch to a private file; preserve the existing hash format.
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'plamenu-source-'));
  const output = path.join(directory, 'patch');
  let descriptor;
  try {
    descriptor = fs.openSync(output, 'wx', 0o600);
    execFileSync('git', ['diff', '--binary', 'HEAD'], {
      cwd:root, stdio:['ignore', descriptor, 'pipe'],
    });
    const patch = fs.readFileSync(output, 'utf8').trim();
    return {source_revision:revision, source_dirty:status !== '',
      source_patch_sha256:digest(patch + status)};
  } finally {
    if (descriptor !== undefined) fs.closeSync(descriptor);
    fs.rmSync(directory, {recursive:true, force:true});
  }
}

export default class EvidenceReporter {
  constructor() {
    this.startedAt = new Date().toISOString();
    this.tests = new Map();
    this.errors = [];
  }
  onBegin(config, suite) {
    this.before = sourceIdentity();
    this.selected = suite.allTests().map(test=>test.id);
    this.projects = config.projects.map(project=>project.name).sort();
    const reportPath = path.resolve(repository, process.env.A11Y_REPORT_PATH || path.join(runDirectory, 'automated.json'));
    fs.mkdirSync(path.dirname(reportPath), {recursive:true});
    // Reserve before execution. Never replace evidence from an earlier run.
    fs.writeFileSync(reportPath, JSON.stringify({schema:2, status:'running', full_pass:false}), {flag:'wx', mode:0o600});
    this.reportPath = reportPath;
    try { this.inventory = loadInventory(repository); }
    catch (error) { this.errors.push(error.message); }
    this.policy = JSON.parse(fs.readFileSync(path.join(repository, 'accessibility/coverage-policy.json')));
  }
  onError(error) { this.errors.push(error.message || String(error)); }
  onTestEnd(test, result) {
    const record = this.tests.get(test.id) || {
      id:test.id, project:test.parent.project()?.name,
      title:test.titlePath().slice(3).join(' > '),
      file:path.relative(path.join(repository, 'accessibility/tests'), test.location.file), attempts:[],
    };
    record.expected_status = test.expectedStatus;
    record.annotations = test.annotations.filter(a=>['skip','fixme','fail'].includes(a.type));
    record.attempts.push({retry:result.retry, status:result.status, duration_ms:result.duration});
    this.tests.set(test.id, record);
  }
  onEnd(result) {
    if (!this.reportPath || !this.before) return {status:'failed'};
    const after = sourceIdentity();
    try {
      if (stable(loadInventory(repository)) !== stable(this.inventory)) this.errors.push('Inventory changed during execution');
    } catch (error) { this.errors.push(error.message); }
    if (stable(this.before) !== stable(after)) this.errors.push('Source changed during execution');
    const tests = [...this.tests.values()];
    const assessment = this.inventory ? assess(this.inventory, this.selected, tests, this.policy)
      : {complete:false, exclusions:[], failures:[]};
    const fullPass = assessment.complete && !assessment.failures.length && !this.errors.length && result.status === 'passed';
    const pendingCleanup = Boolean(process.env.A11Y_FIXTURE_CONTROL_URL);
    let encoderVersion = null;
    try { encoderVersion = execFileSync('ffmpeg', ['-version'], {encoding:'utf8'}).split('\n')[0]; } catch { /* Not a browser coverage requirement. */ }
    const record = {
      schema:2, standard:'WCAG 2.2', level:'AA', ...this.before,
      browser_image:process.env.A11Y_BROWSER_IMAGE || null, encoder_version:encoderVersion,
      base_url:process.env.A11Y_BASE_URL || 'https://plamenu.local',
      started_at:this.startedAt, completed_at:new Date().toISOString(),
      status:fullPass ? (pendingCleanup ? 'pending-cleanup' : 'passed') : assessment.complete ? 'failed' : 'incomplete',
      runner_status:result.status, full_pass:fullPass && !pendingCleanup,
      ...(pendingCleanup ? {fixture_cleanup_pending:true, fixture_tests_pass:fullPass} : {}),
      inventory_sha256:this.inventory ? digest(stable(this.inventory)) : null,
      selected:this.selected, projects:this.projects, tests, ...assessment, errors:this.errors,
    };
    const temporary = this.reportPath + '.tmp';
    fs.writeFileSync(temporary, JSON.stringify(record, null, 2) + '\n', {flag:'wx', mode:0o600});
    fs.renameSync(temporary, this.reportPath);
    console.log(`Browser evidence: ${this.reportPath}\nFull pass: ${record.full_pass}; completed ${tests.length}/${this.inventory?.tests.length || 0}${pendingCleanup ? '; fixture cleanup pending' : ''}`);
    if (this.errors.length || (assessment.complete && !fullPass)) return {status:'failed'};
  }
}
