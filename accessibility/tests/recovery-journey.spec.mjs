import {useFixtureClient} from '../session.mjs';
import { test, expect } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

test.beforeEach(async({page})=>useFixtureClient(page,'recovery'));

async function audit(page, info, name) {
  await page.screenshot({ path: info.outputPath(`${name}.png`), fullPage: true });
  const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa']).analyze();
  expect.soft(result.violations.map(v => ({ id: v.id, nodes: v.nodes.map(n => n.target) }))).toEqual([]);
}

async function mailLink(page, recipient, path) {
  let found;
  await expect.poll(async () => {
    const response = await page.request.get(`${process.env.A11Y_MAILBOX_URL}?recipient=${encodeURIComponent(recipient)}`);
    expect(response.ok()).toBe(true);
    const messages = await response.json();
    found = messages.flatMap(m => m.body.match(/https:\/\/localhost\/\S+/g) || [])
      .filter(url => new URL(url).pathname === path).at(-1);
    return Boolean(found);
  }, { timeout: 20_000 }).toBe(true);
  return found;
}

test('email confirmation and password reset recover from errors and revoke old sessions', async ({ page, browser }, info) => {
  test.setTimeout(120_000);
  const username = `recover_${info.project.name.replaceAll('-', '_')}`;
  const email = `${username}@example.test`;
  const oldPassword = 'Recovery-fixture-old-2026';
  const password = 'Recovery-fixture-new-2026';
  await page.goto('/signup');
  await page.locator('[name=username]').fill(username);
  await page.locator('[name=email]').fill(email);
  await page.locator('[name=password]').fill(oldPassword);
  await page.locator('[name=password_confirmation]').fill(oldPassword);
  await page.locator('[name=date_of_birth]').fill('1990-01-02');
  await page.locator('[name=agreement]').check();
  await page.locator('button[type=submit]').press('Enter');
  await expect(page.getByRole('heading', { name: 'Check your inbox', exact: true })).toBeVisible();
  const confirmation = await mailLink(page, email, '/auth/confirmation');
  await page.goto(confirmation);
  await expect(page.getByRole('heading', { name: 'E-mail confirmed', exact: true })).toBeVisible();
  await page.getByRole('link', { name: 'Sign in', exact: true }).last().press('Enter');
  await page.locator('[name=identifier]').fill(email);
  await page.locator('[name=password]').fill(oldPassword);
  await page.locator('button[type=submit]').press('Enter');
  await page.waitForURL(url => url.pathname === '/');
  const oldSession = page;
  const resetContext = await browser.newContext({ ...info.project.use, baseURL: process.env.A11Y_BASE_URL });
  const reset = await resetContext.newPage();
  try {
    await reset.goto('/auth/password/new');
    await reset.locator('[name=email]').fill(email);
    await reset.locator('button[type=submit]').press('Enter');
    await expect(reset.getByRole('heading', { name: 'Check your inbox', exact: true })).toBeVisible();
    const link = await mailLink(reset, email, '/auth/password/edit');
    await reset.goto(link);
    await reset.locator('[name=password]').fill(password);
    await reset.locator('[name=password_confirmation]').fill(`${password}-wrong`);
    await reset.locator('button[type=submit]').press('Enter');
    await expect(reset.locator('#password-reset-errors')).toBeVisible();
    await audit(reset, info, 'reset-error');
    await expect.soft(reset.locator('#password-reset-errors')).toBeFocused({ timeout: 1500 });
    await expect.soft(reset.locator('#password-reset-errors').getByRole('list')).toHaveCount(1, { timeout: 1500 });
    await expect(reset.locator('[name=password]')).toHaveValue('');
    await expect(reset.locator('[name=password_confirmation]')).toHaveValue('');
    await reset.locator('[name=password]').fill(password);
    await reset.locator('[name=password_confirmation]').fill(password);
    await reset.locator('button[type=submit]').press('Enter');
    await expect(reset.getByRole('heading', { name: 'Password changed', exact: true })).toBeVisible();
    await oldSession.goto('/settings/profile');
    await expect(oldSession).toHaveURL(/\/login/);
    await reset.goto(link);
    await expect(reset.getByRole('heading', { name: 'Invalid reset link', exact: true })).toBeVisible();
    await audit(reset, info, 'reset-consumed');
    await reset.goto('/login');
    await reset.locator('[name=identifier]').fill(email);
    await reset.locator('[name=password]').fill(password);
    await reset.locator('button[type=submit]').press('Enter');
    await reset.waitForURL(url => url.pathname === '/');
  } finally {
    await resetContext.close();
  }
});

test('Russian authentication errors use the interface language', async ({ browser }, info) => {
  const context = await browser.newContext({ ...info.project.use, baseURL: process.env.A11Y_BASE_URL, locale: 'ru-RU' });
  const page = await context.newPage();
  try {
    await page.goto('/login');
    await page.locator('[name=identifier]').fill('nonexistent_recovery_fixture');
    await page.locator('[name=password]').fill('incorrect-fixture-password');
    await page.locator('button[type=submit]').press('Enter');
    await expect(page.locator('#login-error')).toBeVisible();
    await audit(page, info, 'russian-login-error');
    await expect(page.locator('html')).toHaveAttribute('lang', 'ru');
    await expect.soft(page.locator('#login-error')).toContainText('Неверный', { timeout: 1500 });
  } finally {
    await context.close();
  }
});

test('@no-js password reset keeps its error list, focus and recovery path', async ({ page }) => {
  await page.goto('/auth/password/new');
  await page.locator('[name=email]').fill('recovery_no_javascript@localhost');
  await page.locator('button[type=submit]').press('Enter');
  await expect(page.getByRole('heading', { name: 'Check your inbox', exact: true })).toBeVisible();
  const link = await mailLink(page, 'recovery_no_javascript@localhost', '/auth/password/edit');
  await page.goto(link);
  await page.locator('[name=password]').fill('Recovery-no-js-2026');
  await page.locator('[name=password_confirmation]').fill('Recovery-mismatch-2026');
  await page.locator('button[type=submit]').press('Enter');
  await expect(page.locator('#password-reset-errors')).toBeFocused();
  await expect(page.locator('#password-reset-errors').getByRole('list')).toHaveCount(1);
  await page.keyboard.press('Tab');
  await expect(page.locator('[name=password]')).toBeFocused();
  await page.locator('[name=password]').fill('Recovery-no-js-2026');
  await page.locator('[name=password_confirmation]').fill('Recovery-no-js-2026');
  await page.locator('button[type=submit]').press('Enter');
  await expect(page.getByRole('heading', { name: 'Password changed', exact: true })).toBeVisible();
});
