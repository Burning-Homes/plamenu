import { expect, test } from '@playwright/test';
import { randomUUID } from 'node:crypto';

// Identity uses decoded PCM, so changing container metadata cannot isolate imports.
// Generate a real WAV with a unique PCM prefix for each journey.
export async function freshMusicAudio(seconds = 9) {
  const rate = 44100, frames = rate * seconds;
  const buffer = Buffer.alloc(44 + frames * 2);
  buffer.write('RIFF', 0); buffer.writeUInt32LE(buffer.length - 8, 4);
  buffer.write('WAVEfmt ', 8); buffer.writeUInt32LE(16, 16);
  buffer.writeUInt16LE(1, 20); buffer.writeUInt16LE(1, 22);
  buffer.writeUInt32LE(rate, 24); buffer.writeUInt32LE(rate * 2, 28);
  buffer.writeUInt16LE(2, 32); buffer.writeUInt16LE(16, 34);
  buffer.write('data', 36); buffer.writeUInt32LE(frames * 2, 40);
  for (let frame = 0; frame < frames; frame++) {
    buffer.writeInt16LE(Math.round(4096 * Math.sin(2 * Math.PI * 440 * frame / rate)), 44 + frame * 2);
  }
  Buffer.from(randomUUID().replaceAll('-', ''), 'hex').copy(buffer, 44);
  return { name: 'native-audio.wav', mimeType: 'audio/wav', buffer };
}

// WebKit's native audio control does not map Space to play. Exercise its
// visible play button; other engines retain the native keyboard path.
export async function playNativeAudio(audio, info) {
  if (await audio.evaluate(a => a.paused)) {
    if (info.project.name.startsWith('webkit')) {
      const box = await audio.boundingBox();
      // Wide controls add Rewind after metadata loads. Cold preload=none
      // controls still have Play at the left edge, as on narrow controls.
      const loaded = await audio.evaluate(a => Number.isFinite(a.duration) && a.duration > 0);
      await audio.click({ position: { x: box.width >= 400 && loaded ? 56 : 20, y: box.height / 2 } });
    } else await audio.press('Space');
    await expect.poll(() => audio.evaluate(a => a.paused)).toBe(false);
  }
}

// Each worker signs in through the real browser form once. Reuse only its
// session cookie in fresh test contexts: public-page tests remain signed out,
// without creating storage-state files. Do not raise production rate limits
// to accommodate an expanding matrix of tests against one fixture account.
const sessions = new Map();

// Exercise the same native disclosure a keyboard/no-JS author uses. Read the
// open state rather than descendant visibility (which differs across engines).
export async function openVideoOptions(page) {
  const options = page.locator('[data-video-options]');
  if (await options.count() && await options.getAttribute('open') === null) {
    await options.locator(':scope > summary').press('Enter');
  }
  const descriptions = page.locator('details').filter({ has: page.locator('summary', { hasText: 'Descriptions and captions' }) });
  if (await descriptions.count() && await descriptions.getAttribute('open') === null) {
    await descriptions.locator(':scope > summary').press('Enter');
  }
}

export async function createVideoChannel(page, button) {
  const posted=page.waitForResponse(response=>response.request().method()==='POST'
    && new URL(response.url()).pathname==='/web/video-channels');
  await button.press('Enter');
  expect((await posted).status()).toBe(303);
  // A redirect to the current upload URL is not a navigation barrier. Reopen
  // only after commit so subsequent fields cannot target the outgoing form.
  await page.goto('/videos/upload');
  await openVideoOptions(page);
}

export async function signIn(page) {
  const info = test.info();
  // The isolated runner gives each engine its own creator account. Seeded
  // private-draft/catalogue journeys keep their original fixture owner.
  const seeded = /\/(representative|video-drafts|video-catalogue|video-workspace)\.spec\.mjs$/.test(info.file);
  const prefix = process.env.A11Y_CREATOR_PREFIX;
  const username = prefix && !seeded
    ? prefix + info.project.name.replaceAll('-', '_')
    : process.env.A11Y_USERNAME || 'developer';
  await signInAccount(page, username);
}

// Separate fixture actors are real accounts; reuse their browser sessions
// rather than exhausting login budgets by treating every new context as a user.
export function participantUsername(info = test.info()) {
  return process.env.A11Y_PARTICIPANT_PREFIX
    ? process.env.A11Y_PARTICIPANT_PREFIX + info.project.name.replaceAll('-', '_')
    : 'channel_editor';
}

export async function useFixtureClient(page, role) {
  if (!process.env.A11Y_CANONICAL_ORIGIN) return;
  if (!['participant', 'security', 'recovery', 'throttle'].includes(role)) throw new Error('Unknown fixture client role');
  const info = test.info();
  await page.context().setExtraHTTPHeaders({
    ...info.project.use.extraHTTPHeaders,
    'X-Plamenu-Fixture-Client': `${info.project.name}-${role}`,
  });
}

export async function signInParticipant(page) {
  await useFixtureClient(page, 'participant');
  await signInAccount(page, participantUsername());
}

async function signInAccount(page, username) {
  const info = test.info();
  const key = `${info.project.name}:${username}`;
  const cached = sessions.get(key);
  if (cached) {
    await page.context().addCookies(cached);
    await page.goto('/');
    await expect(page).toHaveURL(url => url.pathname === '/');
  } else {
    await page.goto('/login');
    await page.locator('input[name="identifier"]').fill(username);
    await page.locator('input[name="password"]').fill(process.env.A11Y_PASSWORD || 'plamenu-development-only');
    await Promise.all([
      page.waitForURL(url => url.pathname === '/'),
      page.locator('button[type="submit"]').click(),
    ]);
    const cookies = (await page.context().cookies()).filter(cookie => cookie.name === '__Host-plamenu_session');
    expect(cookies).toHaveLength(1);
    sessions.set(key, cookies);
  }
  await expect(page.locator('a.compose-btn[href="/compose"]').first()).toBeAttached();
}

// Standalone creation is also reachable when an account already has channels.
export async function videoChannelForm(page) {
  const form=page.locator('form[action="/web/video-channels"]');
  if (!await form.count()) {
    const link=page.getByRole('link',{name:'Create a video channel',exact:true}).first();
    if(await link.count()) await link.press('Enter');
    else await page.goto('/videos/channels/new');
    await expect(page).toHaveURL(url=>url.pathname==='/videos/channels/new');
  }
  await openDetailsFor(form);
  return form;
}

// Source journeys also work in isolation, before another test has created a channel.
export async function ensureVideoFormChannel(page, form) {
  if (await form.count()) return;
  const returnUrl = page.url();
  await page.getByRole('link', { name: 'Create a video channel', exact: true }).first().press('Enter');
  const channel = await videoChannelForm(page);
  const name = `source_${randomUUID().replaceAll('-', '').slice(0, 20)}`;
  await channel.getByLabel('Channel address (letters, numbers and underscores)', { exact: true }).fill(name);
  await channel.getByLabel('Channel name', { exact: true }).fill('Source browser fixture');
  await createVideoChannel(page, channel.getByRole('button', { name: 'Create a video channel', exact: true }));
  await page.goto(returnUrl);
  await expect(form).toBeVisible();
}

export async function openDetailsFor(control) {
  // The locator must resolve inside collapsed details (role queries need includeHidden).
  await expect(control).toBeAttached();
  const ancestors=await control.locator('xpath=ancestor::details').all();
  for (const details of ancestors) {
    if (await details.getAttribute('open')===null) await details.locator(':scope > summary').press('Enter');
  }
}

// Operate the existing human-readable picker; the native select remains the no-JS path.
export async function chooseVideoLanguage(form, value) {
  const select=form.locator('select[name="language"]');
  await openDetailsFor(select);
  if (await select.isVisible()) {
    await select.selectOption(value);
  } else {
    const combo=select.locator('xpath=ancestor::*[@data-compose-combo]');
    await combo.locator('[data-combo-trigger]').press('Enter');
    await combo.locator('[data-combo-search]').fill(value);
    await combo.locator(`[role="option"][data-value="${value}"]`).click();
  }
  await expect(select).toHaveValue(value);
}

// Use the visible keyboard picker; saved account locale outranks browser locale.
export async function chooseInterfaceLanguage(page, code) {
  await page.goto('/settings/languages');
  const form = page.locator('form[action="/web/settings/languages"]');
  const picker = form.locator('[data-compose-combo]').filter({ has: page.locator('select[name=locale]') });
  await picker.locator('[data-combo-trigger]').press('Enter');
  await picker.locator('[data-combo-search]').fill(code);
  await picker.locator('[data-combo-search]').press('Enter');
  await expect(form.locator('select[name=locale]')).toHaveValue(code);
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toBeVisible();
}
