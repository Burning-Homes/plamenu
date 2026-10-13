import {freshMusicAudio} from '../session.mjs';
import { test, expect } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import { randomUUID } from 'node:crypto';

async function audit(page, noScript) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  if (!noScript) {
    const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
    expect(result.violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) }))).toEqual([]);
  }
}
async function submit(page, button) {
  const action = await button.evaluate(control => new URL(control.form.action).pathname);
  const posted = page.waitForResponse(response => response.request().method() === 'POST'
    && new URL(response.url()).pathname === action);
  await button.press('Enter');
  const response = await posted;
  expect(response.status()).toBe(303);
  const location = response.headers().location;
  expect(location).toBeTruthy();
  await page.goto(location);
  return location;
}
async function journey(page, browser, info, noScript) {
  test.setTimeout(180000);
  // Dedicated disposable identities: deletion cannot disrupt other journeys.
  const username = 'music_erasure_' + info.project.name.replaceAll('-', '_');
  const password = process.env.A11Y_PASSWORD;
  await page.goto('/login');
  await page.locator('input[name="identifier"]').fill(username);
  await page.locator('input[name="password"]').fill(password);
  expect(await submit(page, page.locator('button[type="submit"]'))).toBe('/');
  await page.goto('/music/artists/new');
  await page.getByLabel('Name', { exact: true }).fill('Unpublished biography ' + randomUUID());
  await page.getByLabel('Description', { exact: true }).fill('Private notes without any retained track.');
  await submit(page, page.getByRole('button', { name: 'Create artist', exact: true }));
  const artistUrl = page.url();
  await page.getByRole('link', { name: 'Create album', exact: true }).press('Enter');
  await page.waitForURL(url => url.pathname === '/music/albums/new');
  await page.getByLabel('Name', { exact: true }).fill('Unpublished album ' + randomUUID());
  await page.getByLabel('Description', { exact: true }).fill('Private album notes.');
  await submit(page, page.getByRole('button', { name: 'Create album', exact: true }));
  const albumUrl = page.url();
  await page.goto('/music/assets?mine=true');
  await page.getByText('Upload audio', { exact: true }).first().press('Enter');
  await page.getByLabel('Audio file', { exact: true }).setInputFiles(await freshMusicAudio());
  await submit(page, page.getByRole('button', { name: 'Upload audio', exact: true }));
  const assetUrl = page.url();
  const assetId = new URL(assetUrl).pathname.split('/').at(-1);
  const streamUrl = new URL(`/music/assets/${assetId}/stream`, assetUrl).href;
  await page.getByText('Edit track', { exact: true }).press('Enter');
  const edit = page.locator(`form[action="/web/music/assets/${assetId}/edit"]`);
  await edit.getByLabel('Title', { exact: true }).fill('Recording removed with its account');
  await edit.getByLabel('Who can listen', { exact: true }).selectOption('public');
  await submit(page, edit.getByRole('button', { name: 'Save track', exact: true }));
  await page.goto('/music/radios?mine=true');
  await page.locator('summary').filter({ hasText: /^Create station$/ }).press('Enter');
  await page.getByLabel('Station name', { exact: true }).fill('Station removed with its account');
  await page.getByLabel('Station settings', { exact: true }).fill('private_erasure_configuration');
  await page.getByLabel('Who can view', { exact: true }).selectOption('public');
  await submit(page, page.getByRole('button', { name: 'Create station', exact: true }));
  const radioUrl = page.url();
  const anonymous = await browser.newContext({ baseURL: process.env.A11Y_BASE_URL, ignoreHTTPSErrors: false, javaScriptEnabled: !noScript });
  try {
    expect((await anonymous.request.get(radioUrl)).status()).toBe(200);
    const range = await anonymous.request.get(streamUrl, { headers: { Range: 'bytes=0-31' } });
    expect(range.status()).toBe(206);
    expect((await range.body()).length).toBe(32);
    await page.goto('/settings/account');
    const deletion = page.locator('form[action="/web/settings/account/delete"]');
    await expect(deletion.locator('input[name="confirm_handle"]')).toBeVisible();
    await audit(page, noScript);
    await page.screenshot({ path: info.outputPath('music-account-erasure.png'), fullPage: true });
    const invalid = await page.request.post('/web/settings/account/delete', { form: { csrf: 'invalid', current_password: password, confirm_handle: '@' + username }, maxRedirects: 0 });
    expect(invalid.status()).toBe(403);
    expect((await anonymous.request.get(streamUrl)).status()).toBe(200);
    await deletion.locator('input[name="confirm_handle"]').fill('@' + username);
    await deletion.locator('input[name="current_password"]').fill(password);
    expect(await submit(page, deletion.locator('button[type="submit"]'))).toBe('/');
    expect((await page.context().cookies()).filter(cookie => cookie.name === '__Host-plamenu_session')).toHaveLength(0);
    expect((await page.context().cookies()).filter(cookie => cookie.name === '__Host-plamenu_accounts')).toHaveLength(0);
    // Entity pages apply the instance's disabled anonymous-preview policy
    // before looking up a resource. Check that gate without following it.
    for (const url of [artistUrl, albumUrl]) {
      const response = await anonymous.request.get(url, { maxRedirects: 0 });
      expect(response.status()).toBe(303);
      expect(response.headers().location).toBe('/login');
    }
    for (const url of [assetUrl, radioUrl, streamUrl]) {
      expect((await anonymous.request.get(url)).status()).toBe(404);
    }
    expect((await anonymous.request.get(streamUrl, { headers: { Range: 'bytes=0-31' } })).status()).toBe(404);
    await page.goto('/login');
    await expect(page.getByRole('heading', { name: 'Choose an account', exact: true })).toHaveCount(0);
    await page.locator('input[name="identifier"]').fill(username);
    await page.locator('input[name="password"]').fill(password);
    const rejectedLogin = page.waitForResponse(response => response.request().method() === 'POST'
      && new URL(response.url()).pathname === '/login');
    await page.locator('button[type="submit"]').press('Enter');
    expect((await rejectedLogin).status()).toBe(401);
    await expect(page.getByRole('alert')).toBeVisible();
    await expect(page).toHaveURL(url => url.pathname === '/login');
    expect((await page.context().cookies()).filter(cookie => cookie.name === '__Host-plamenu_session')).toHaveLength(0);
    await audit(page, noScript);
    await page.screenshot({ path: info.outputPath('music-erased-account-login.png'), fullPage: true });
  } finally {
    await anonymous.close();
  }
}
test('account deletion revokes music and removes unpublished metadata', async ({ page, browser }, info) => journey(page, browser, info, false));
test('account deletion revokes music without JavaScript @no-js', async ({ page, browser }, info) => journey(page, browser, info, true));
