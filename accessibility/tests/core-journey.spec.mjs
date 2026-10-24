import {signInParticipant} from '../session.mjs';
import { test, expect } from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import { openDetailsFor, signIn } from '../session.mjs';

async function evidence(page, info, name) {
  await page.screenshot({ path: info.outputPath(`${name}.png`), fullPage: true });
  const state = await page.evaluate(() => ({
    focus: document.activeElement?.id || document.activeElement?.getAttribute('name'),
    agreement: document.querySelector('[name=agreement]')?.checked,
    birthday: document.querySelector('[name=date_of_birth]')?.value,
    identifier: document.querySelector('[name=identifier]')?.value,
    overflow: document.documentElement.scrollWidth > document.documentElement.clientWidth,
  }));
  await info.attach(name, { body: JSON.stringify(state, null, 2), contentType: 'application/json' });
  console.log(name, state);
  expect.soft(state.overflow, `${name}: no page overflow`).toBe(false);
  const scan = await new AxeBuilder({ page }).withTags([
    'wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa',
  ]).analyze();
  expect.soft(scan.violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) })), name).toEqual([]);
}

test('signup error recovery, login and first post form a complete core journey', async ({ page }, info) => {
  test.setTimeout(120_000);
  const username = `journey_${info.project.name.replaceAll('-', '_')}`;
  const password = 'Journey-test-only-2026';
  await page.goto('/signup');
  await expect(page.locator('form[action="/signup"]')).toBeVisible();
  await expect(page.locator('[name=agreement]')).not.toBeChecked();
  await expect(page.locator('[name=date_of_birth]')).toHaveValue('');
  await page.locator('[name=username]').fill(username);
  // Email is optional in Plamenu. This path exercises real instant signup;
  // email delivery/confirmation remains a separately documented test boundary.
  await page.locator('[name=password]').fill(password);
  await page.locator('[name=password_confirmation]').fill(`${password}-mismatch`);
  await page.locator('[name=date_of_birth]').fill('1990-01-02');
  await page.locator('[name=time_zone]').selectOption('Europe/Paris');
  await page.locator('[name=agreement]').check();
  await Promise.all([
    page.waitForResponse(r => r.request().method() === 'POST' && new URL(r.url()).pathname === '/signup'),
    page.locator('button[type=submit]').press('Enter'),
  ]);
  await expect(page.locator('#signup-errors')).toBeVisible();
  await evidence(page, info, 'signup-error');
  expect.soft(await page.locator('[name=date_of_birth]').inputValue()).toBe('1990-01-02');
  expect.soft(await page.locator('[name=agreement]').isChecked()).toBe(true);
  await expect.soft(page.locator('#signup-errors')).toBeFocused({ timeout: 2000 });
  await expect(page.locator('#signup-errors').getByRole('list')).toHaveCount(1);
  await expect(page.locator('[name=password]')).toHaveValue('');
  await expect(page.locator('[name=password_confirmation]')).toHaveValue('');
  await page.keyboard.press('Tab');
  await expect(page.locator('[name=username]')).toBeFocused();
  await expect(page.locator('[name=password_confirmation]')).toHaveAttribute('aria-invalid', 'true');
  await expect(page.locator('[name=username]')).toHaveValue(username);
  await expect(page.locator('[name=time_zone]')).toHaveValue('Europe/Paris');
  await page.locator('[name=password]').fill(password);
  await page.locator('[name=password_confirmation]').fill(password);
  await Promise.all([
    page.waitForURL(url => url.pathname === '/'),
    page.locator('button[type=submit]').press('Enter'),
  ]);
  if (!await page.locator('form[action="/logout"] button:visible').count()) {
    await page.locator('details[data-drawer] > summary').press('Enter');
  }
  await Promise.all([
    page.waitForURL(url => url.pathname === '/login'),
    page.locator('form[action="/logout"] button:visible').press('Enter'),
  ]);
  await page.locator('[name=identifier]').fill(username);
  await page.locator('[name=password]').fill(`${password}-wrong`);
  await Promise.all([
    page.waitForResponse(r => r.request().method() === 'POST' && new URL(r.url()).pathname === '/login'),
    page.locator('button[type=submit]').press('Enter'),
  ]);
  await expect(page.locator('#login-error')).toBeVisible();
  await evidence(page, info, 'login-error');
  await expect.soft(page.locator('[name=identifier]')).toHaveValue(username, { timeout: 2000 });
  await expect.soft(page.locator('#login-error')).toBeFocused({ timeout: 2000 });
  await expect(page.locator('[name=password]')).toHaveValue('');
  await page.locator('[name=password]').fill(password);
  await Promise.all([
    page.waitForURL(url => url.pathname === '/'),
    page.locator('button[type=submit]').press('Enter'),
  ]);
  await page.goto('/compose');
  await page.locator('textarea[name=status]').fill('My first accessible Plamenu post.');
  await page.locator('button[name=op][value=post]').press('Enter');
  await page.waitForURL(url => url.pathname.startsWith(`/@${username}/`));
  await evidence(page, info, 'first-post');
  for (const action of ['favourite', 'bookmark']) {
    const button = page.locator(`form[data-action=${action}] button`).first();
    await button.press('Enter');
    await expect(button).toHaveClass(/\bis-active\b/);
    await expect(button).toBeEnabled();
    await button.press('Enter');
    await expect(button).not.toHaveClass(/\bis-active\b/);
  }
});

test('post action failure is explained and can be retried with the keyboard', async ({ page, browser }, info) => {
  await signIn(page);
  await page.goto('/compose');
  await page.locator('textarea[name=status]').fill('Action failure recovery fixture.');
  await page.locator('button[name=op][value=post]').press('Enter');
  await page.waitForURL(url => /\/@[^/]+\/\d+/.test(url.pathname));
  const form = page.locator('form[data-action=favourite]').first();
  const button = form.locator('button');
  await expect(button).toHaveAccessibleName('Favourite (0)');
  await expect(page.locator('.status__actions a[title="Reply"]').first()).toHaveAccessibleName('Reply (0)');
  await expect(page.locator('.status__actions a[title="Quote"]').first()).toHaveAccessibleName('Quote (0)');
  await expect(page.locator('form[data-action=reblog] button').first()).toHaveAccessibleName('Boost (0)');
  const action = await form.getAttribute('action');
  await page.route(`**${action}`, route => route.fulfill({ status: 503, body: 'Unavailable' }));
  await button.press('Enter');
  await expect(button).toBeEnabled();
  await expect(button).not.toHaveClass(/\bis-active\b/);
  await expect(button).toHaveAccessibleName('Favourite (0)');
  await evidence(page, info, 'action-failure');
  await expect.soft(page.locator('.status__action-error')).toBeVisible({ timeout: 2000 });
  await expect(page.locator('.status__action-error')).toHaveAttribute('role', 'alert');
  await expect(button).toBeFocused();
  await page.unroute(`**${action}`);
  await button.press('Enter');
  await expect(button).toHaveClass(/\bis-active\b/);
  await expect(button).toHaveAccessibleName('Remove favourite (1)');
  await expect(button).toBeEnabled();
  await expect(button).toBeFocused();
  await expect(page.locator('.status__action-error')).toHaveCount(0);
  await page.reload();
  await expect(button).toHaveClass(/\bis-active\b/);
  await expect(button).toHaveAccessibleName('Remove favourite (1)');
  const anonymous = await browser.newContext({ ...info.project.use, baseURL: process.env.A11Y_BASE_URL });
  try {
    const publicPage = await anonymous.newPage();
    await publicPage.goto(new URL(page.url()).pathname);
    await expect(publicPage.getByRole('link', { name: 'Favourite from your own server (1)', exact: true })).toBeVisible();
    await expect(publicPage.getByRole('link', { name: 'Boost from your own server (0)', exact: true })).toBeVisible();
    await expect(publicPage.getByRole('link', { name: 'Quote from your own server (0)', exact: true })).toBeVisible();
    await info.attach('action-names', { body: await publicPage.locator('.status__actions').first().ariaSnapshot(), contentType: 'text/plain' });
  } finally {
    await anonymous.close();
  }
  // The shared action handler must also recover from a dropped connection.
  for (const name of ['bookmark', 'reblog']) {
    const other = page.locator(`form[data-action=${name}]`).first();
    const control = other.locator('button');
    const path = await other.getAttribute('action');
    await page.route(`**${path}`, route => route.abort('failed'));
    await control.press('Enter');
    await expect(page.locator('.status__action-error')).toBeVisible();
    await expect(control).toBeEnabled();
    await expect(control).toHaveAttribute('aria-pressed', 'false');
    await expect(control).toBeFocused();
    await page.unroute(`**${path}`);
    await control.press('Enter');
    await expect(control).toBeEnabled();
    await expect(control).toHaveAttribute('aria-pressed', 'true');
    await expect(page.locator('.status__action-error')).toHaveCount(0);
  }
  // A response arriving later must not steal focus from subsequent navigation.
  let release, reached;
  const gate = new Promise(resolve => { release = resolve; });
  const requested = new Promise(resolve => { reached = resolve; });
  const undo = await form.getAttribute('action');
  await page.route(`**${undo}`, async route => {
    reached();
    await gate;
    await route.fulfill({ status: 503, body: 'Unavailable' });
  });
  await button.press('Enter');
  await requested;
  const next = page.locator('.skip-link');
  await next.focus();
  release();
  await expect(button).toBeEnabled();
  await expect(button).toHaveAttribute('aria-pressed', 'true');
  await expect(button).toHaveAccessibleName('Remove favourite (1)');
  await expect(next).toBeFocused();
});

test('@no-js login error recovery and publishing work without scripts', async ({ page }, info) => {
  const username = `a11y_${info.project.name.replaceAll('-', '_')}`;
  await page.goto('/login');
  await page.locator('[name=identifier]').fill(username);
  await page.locator('[name=password]').fill('incorrect-fixture-password');
  await page.locator('button[type=submit]').press('Enter');
  await expect(page.locator('#login-error')).toBeFocused();
  await expect(page.locator('[name=identifier]')).toHaveValue(username);
  await expect(page.locator('[name=password]')).toHaveValue('');
  await page.locator('[name=password]').fill(process.env.A11Y_PASSWORD);
  await page.locator('button[type=submit]').press('Enter');
  await page.waitForURL(url => url.pathname === '/');
  await page.goto('/compose');
  await page.locator('textarea[name=status]').fill('Keyboard posting without JavaScript.');
  await page.locator('button[name=op][value=post]').press('Enter');
  await page.waitForURL(url => url.pathname.startsWith(`/@${username}/`));
  const favourite = page.locator('form[data-action=favourite] button').first();
  await expect(favourite).toHaveAccessibleName('Favourite (0)');
  await favourite.press('Enter');
  await expect(favourite).toHaveAttribute('aria-pressed', 'true');
  await expect(favourite).toHaveAccessibleName('Remove favourite (1)');
  await favourite.press('Enter');
  await expect(favourite).toHaveAttribute('aria-pressed', 'false');
  await expect(favourite).toHaveAccessibleName('Favourite (0)');
});

test('follow, home reading, content warnings, poll voting, replies and notifications', async ({ page, browser }, info) => {
  test.setTimeout(120_000);
  await signIn(page);
  const username = `a11y_${info.project.name.replaceAll('-', '_')}`;
  const readerContext = await browser.newContext({
    ...info.project.use,
    baseURL: process.env.A11Y_BASE_URL,
  });
  const reader = await readerContext.newPage();
  try {
    await signInParticipant(reader);
    await reader.goto(`/@${username}`);
    await reader.locator('form.follow button').press('Enter');
    await expect(reader.locator('form.follow button')).toHaveText('Unfollow');

    const text = `Community poll for ${info.project.name}.`;
    await page.goto('/compose');
    await page.locator('textarea[name=status]').fill(text);
    await page.locator('[data-compose-toggle=cw]').press('Enter');
    await page.locator('[name=spoiler_text]').fill('A small community question');
    await page.locator('[data-compose-toggle=poll]').press('Enter');
    await page.locator('[name="poll_options[]"]').nth(0).fill('Morning');
    await page.locator('[name="poll_options[]"]').nth(1).fill('Evening');
    await page.locator('button[name=op][value=post]').press('Enter');
    await page.waitForURL(url => url.pathname.startsWith(`/@${username}/`));
    const permalink = new URL(page.url()).pathname;
    await reader.goto('/');
    await expect(reader.locator('.status__cw > summary').filter({ hasText: 'A small community question' }).first()).toBeVisible();
    await reader.goto(permalink);
    await reader.locator('.status__cw > summary').first().press('Enter');
    await expect(reader.getByText(text, { exact: true })).toBeVisible();
    await reader.locator('.poll input[value="0"]').press('Space');
    await reader.locator('.poll__vote').press('Enter');
    await expect(reader.locator('.poll__result.is-own')).toContainText('Morning');
    await evidence(reader, info, 'poll-voted');
    await reader.locator('textarea[name=status]').fill(`@${username} Morning works for me.`);
    await reader.locator('button[name=op][value=post]').press('Enter');
    const reply = reader.getByText(`@${username} Morning works for me.`, { exact: false }).first();
    // Replies inherit the parent's warning; open it through the native UI.
    await openDetailsFor(reply);
    await expect(reply).toBeVisible();
    await page.goto('/notifications');
    await expect(page.locator('.notification__label').filter({ hasText: 'mentioned you' }).first()).toBeVisible();
    await evidence(page, info, 'notifications');
    await reader.goto(`/@${username}`);
    await reader.locator('form.follow button').press('Enter');
    await expect(reader.locator('form.follow button')).toHaveText('Follow');
  } finally {
    await readerContext.close();
  }
});
