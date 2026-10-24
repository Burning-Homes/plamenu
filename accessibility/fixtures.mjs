import {test as base} from '@playwright/test';
import {randomUUID} from 'node:crypto';
import {clearSessions} from './session.mjs';

export * from '@playwright/test';

export const test = base.extend({
  coldRendition: [false, {option: true}],
  _isolatedServer: [async ({coldRendition}, use, info) => {
    const url = process.env.A11Y_FIXTURE_CONTROL_URL;
    if (!url) throw new Error('Run browser journeys through ./dev a11y: an owned fixture is required');
    const owner = `${info.testId}:${info.retry}:${randomUUID()}`;
    const transition = async action => {
      const response = await fetch(`${url}/${action}`, {
        method: 'POST', body: JSON.stringify({case: owner, cold_rendition: coldRendition}),
        headers: {'Content-Type': 'application/json'}, signal: AbortSignal.timeout(100_000),
      });
      if (!response.ok) throw new Error(`Browser fixture ${action} failed: ${await response.text()}`);
    };
    clearSessions();
    await transition('begin');
    try { await use({release: () => transition('release-encoder')}); }
    finally {
      clearSessions();
      await transition('end');
    }
  }, {auto: true, timeout: 120_000}],
  coldEncoding: async ({_isolatedServer, coldRendition}, use) => {
    if (!coldRendition) throw new Error('Cold encoding requires the explicit coldRendition fixture option');
    await use(_isolatedServer);
  },
});
