import { test, expect } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import { signIn } from '../session.mjs';

async function form(page, operation) {
  return page.locator(`form:has(input[name="operation"][value="${operation}"])`);
}
async function scan(page, noScript) {
  const widths = await page.evaluate(() => [innerWidth, document.documentElement.scrollWidth]);
  expect(widths[1]).toBeLessThanOrEqual(widths[0]);
  if (!noScript) {
    const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
    expect(result.violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) }))).toEqual([]);
  }
}

async function journey(page, browser, info, noScript) {
  test.setTimeout(120_000);
  await signIn(page);
  await page.goto('/videos');
  const name = `team_${Date.now().toString(36)}_${info.project.name.slice(0, 3)}`;
  const create = page.locator('form[action="/web/video-channels"]');
  if (!await create.isVisible()) await create.locator('..').locator(':scope > summary').click();
  await create.getByLabel('Channel address').fill(name);
  await create.getByLabel('Channel name').fill(`Team ${name}`);
  await create.getByRole('button', { name: 'Create a video channel' }).press('Enter');
  await page.waitForURL(url => url.pathname === '/videos');
  await page.getByRole('link', { name: 'Manage video channels', exact: true }).first().click();
  await page.getByRole('link', { name: `Team ${name} (@${name})`, exact: true }).click();
  const channelPath = new URL(page.url()).pathname;
  await scan(page, noScript);
  let profile = await form(page, 'profile');
  await profile.getByLabel('Channel name').fill(`Team revised ${name}`);
  await profile.getByRole('textbox', { name: 'Description', exact: true }).fill('A team sharing accessible films.');
  await profile.getByRole('button', { name: 'Save channel profile' }).press('Enter');
  await expect(page.getByRole('heading', { level: 1 })).toContainText(`Team revised ${name}`);
  let invite = await form(page, 'invite');
  await invite.getByLabel("Local person's username").fill('channel_editor');
  await invite.getByRole('combobox', { name: 'Role', exact: true }).selectOption('editor');
  await invite.getByRole('button', { name: 'Invite collaborator' }).press('Enter');
  await expect(page.getByText('Awaiting acceptance', { exact: false }).first()).toBeVisible();

  const context = await browser.newContext({
    baseURL: process.env.A11Y_BASE_URL || 'https://plamenu.local', ignoreHTTPSErrors: true,
    javaScriptEnabled: !noScript, viewport: info.project.use.viewport,
    ...(process.env.A11Y_CANONICAL_ORIGIN ? { extraHTTPHeaders: { Origin: process.env.A11Y_CANONICAL_ORIGIN } } : {}),
  });
  const recipient = await context.newPage();
  try {
    await recipient.goto('/login');
    await recipient.locator('input[name="identifier"]').fill('channel_editor');
    await recipient.locator('input[name="password"]').fill('plamenu-development-only');
    await recipient.getByRole('button', { name: 'Sign in', exact: true }).click();
    await recipient.waitForURL(url => url.pathname === '/');
    await recipient.goto('/videos/channels');
    await recipient.getByRole('link', { name: `Team revised ${name} (@${name})`, exact: true }).click();
    await expect(await form(recipient, 'profile')).toHaveCount(0);
    await scan(recipient, noScript);
    await (await form(recipient, 'accept')).getByRole('button', { name: 'Accept invitation' }).press('Enter');
    await expect(recipient.getByText('Your role: Editor', { exact: true })).toBeVisible();
    await expect(await form(recipient, 'invite')).toHaveCount(0);
    profile = await form(recipient, 'profile');
    await profile.getByRole('textbox', { name: 'Description', exact: true }).fill('Corrected by a consenting editor.');
    await profile.getByRole('button', { name: 'Save channel profile' }).press('Enter');
    await page.reload();
    await expect((await form(page, 'profile')).getByRole('textbox', { name: 'Description', exact: true })).toHaveValue('Corrected by a consenting editor.');
    await page.getByRole('button', { name: 'Revoke access for @channel_editor', exact: true }).press('Enter');
    expect((await recipient.request.get(channelPath)).status()).toBe(404);
    await page.locator('summary').filter({ hasText: /^Transfer ownership$/ }).press('Enter');
    let transfer = await form(page, 'transfer');
    await transfer.getByLabel("Local person's username").fill('channel_editor');
    await transfer.getByRole('button', { name: 'Request ownership transfer' }).press('Enter');
    await recipient.goto('/videos/channels');
    await recipient.getByRole('link', { name: `Team revised ${name} (@${name})`, exact: true }).click();
    await expect(recipient.getByRole('heading', { name: 'Ownership transfer invitation', exact: true })).toBeVisible();
    transfer = await form(recipient, 'accept_transfer');
    await transfer.getByLabel(`Type the channel address to confirm (@${name})`).fill(name);
    await scan(recipient, noScript);
    await transfer.getByRole('button', { name: 'Accept ownership' }).press('Enter');
    await expect(recipient.getByText('Your role: Owner', { exact: true })).toBeVisible();
    expect((await page.request.get(channelPath)).status()).toBe(404);
    await scan(recipient, noScript);
    await recipient.screenshot({ path: test.info().outputPath('channel-owner.png'), fullPage: true });
    await recipient.locator('summary').filter({ hasText: /^Delete channel$/ }).press('Enter');
    const deletion = await form(recipient, 'delete');
    await deletion.getByLabel(`Type the channel address to confirm (@${name})`).fill(name);
    await deletion.getByRole('button', { name: 'Delete channel', exact: true }).press('Enter');
    await recipient.waitForURL(url => url.pathname === '/videos/channels');
    expect((await recipient.request.get(channelPath)).status()).toBe(404);
  } finally { await context.close(); }
}

test('Channel roles and ownership are keyboard-operable and responsive', async ({ page, browser }, info) => journey(page, browser, info, false));
test('Channel roles and ownership work without scripts @no-js', async ({ page, browser }, info) => journey(page, browser, info, true));
