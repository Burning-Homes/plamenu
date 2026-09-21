import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const repository = path.resolve(here, '..');

export default class EvidenceReporter {
  constructor() {
    this.startedAt = new Date().toISOString();
    this.projects = new Map();
    this.tests = [];
  }

  onBegin(config) {
    for (const project of config.projects) {
      this.projects.set(project.name, { passed: 0, failed: 0, skipped: 0 });
    }
  }

  onTestEnd(test, result) {
    const project = test.parent.project()?.name || 'unknown';
    const status = result.status === 'passed'
      ? 'passed'
      : result.status === 'skipped'
        ? 'skipped'
        : 'failed';
    const counts = this.projects.get(project) || { passed: 0, failed: 0, skipped: 0 };
    counts[status] += 1;
    this.projects.set(project, counts);
    this.tests.push({
      project,
      title: test.titlePath().slice(1).join(' > '),
      status,
      duration_ms: result.duration,
    });
  }

  onEnd(result) {
    const reportPath = path.resolve(
      repository,
      process.env.A11Y_REPORT_PATH || 'target/accessibility/automated.json',
    );
    const revision = execFileSync('git', ['rev-parse', 'HEAD'], {
      cwd: repository,
      encoding: 'utf8',
    }).trim();
    const dirty = execFileSync('git', ['status', '--porcelain'], {
      cwd: repository,
      encoding: 'utf8',
    }).trim() !== '';
    const record = {
      schema: 1,
      standard: 'WCAG 2.2',
      level: 'AA',
      source_revision: revision,
      source_dirty: dirty,
      base_url: process.env.A11Y_BASE_URL || 'https://plamenu.local',
      started_at: this.startedAt,
      completed_at: new Date().toISOString(),
      status: result.status,
      projects: Object.fromEntries([...this.projects.entries()].sort()),
      tests: this.tests,
    };
    fs.mkdirSync(path.dirname(reportPath), { recursive: true });
    fs.writeFileSync(reportPath, `${JSON.stringify(record, null, 2)}\n`, { mode: 0o600 });
  }
}
