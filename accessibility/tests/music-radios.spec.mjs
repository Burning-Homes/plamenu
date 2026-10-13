import { test, expect } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import { signIn, openDetailsFor } from '../session.mjs';
import { randomUUID } from 'node:crypto';

async function audit(page, noScript) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  if (!noScript) {
    const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
    expect(result.violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) }))).toEqual([]);
  }
}
async function submit(page, button, destination) {
  const action = await button.evaluate(control => new URL(control.form.action).pathname);
  const posted = page.waitForResponse(response => response.request().method() === 'POST'
    && new URL(response.url()).pathname === action);
  await button.press('Enter');
  const response = await posted;
  expect(response.status()).toBe(303);
  expect(response.headers().location).toBe(destination);
  // The redirect can return to the same URL. Wait for commit before reading
  // permissions through another context, then load that exact saved document.
  await page.goto(destination);
}
async function journey(page, browser, info, noScript) {
  test.setTimeout(120000);
  await signIn(page);
  await page.goto('/music');
  const radioLink=page.getByRole('link', { name: 'Radio stations', exact: true, includeHidden: true });
  await openDetailsFor(radioLink); await radioLink.press('Enter');
  await page.locator('summary').filter({ hasText: /^Create station$/ }).press('Enter');
  const name = 'Evening station <safe> ' + randomUUID();
  await page.getByLabel('Station name', { exact: true }).fill(name);
  await page.getByLabel('Description', { exact: true }).fill('A station configuration shared with a local member.');
  await page.getByLabel('Station settings', { exact: true }).fill('{"genre":"ambient","private_setting":"fixture-only"}');
  await expect(page.getByLabel('Who can view', { exact: true })).toHaveValue('private');
  await audit(page, noScript);
  await page.screenshot({ path: info.outputPath('music-radio-create.png'), fullPage: true });
  await page.getByRole('button', { name: 'Create station', exact: true }).press('Enter');
  await page.waitForURL(url => /^\/music\/radios\/\d+$/.test(url.pathname));
  const stationUrl = page.url();
  const anonymous = await browser.newContext({ baseURL: process.env.A11Y_BASE_URL, ignoreHTTPSErrors: false, javaScriptEnabled: !noScript });
  const viewer = await browser.newContext({ baseURL: process.env.A11Y_BASE_URL, ignoreHTTPSErrors: false, javaScriptEnabled: !noScript, extraHTTPHeaders: info.project.use.extraHTTPHeaders });
  try {
    expect((await anonymous.request.get(stationUrl)).status()).toBe(404);
    const invalid = await page.request.post('/web/music/radios/create', { form: { csrf: 'invalid', name: 'Must not exist', visibility: 'public' } });
    expect(invalid.status()).toBe(403);
    await page.getByText('Edit station', { exact: true }).press('Enter');
    const edit = page.locator('form[action$="/edit"]');
    await edit.getByLabel('Who can view', { exact: true }).selectOption('public');
    await submit(page, edit.getByRole('button', { name: 'Save station settings', exact: true }), new URL(stationUrl).pathname);
    const publicView = await anonymous.request.get(stationUrl);
    expect(publicView.status()).toBe(200);
    expect(await publicView.text()).not.toContain('private_setting');
    const publicPage = await anonymous.newPage();
    await publicPage.goto(stationUrl);
    await expect(publicPage.getByRole('heading', { name, exact: true })).toBeVisible();
    await audit(publicPage, noScript);
    await publicPage.goto('/music/radios');
    // The disposable instance starts with anonymous directory previews disabled.
    await expect(publicPage).toHaveURL(url => url.pathname === '/login');
    await publicPage.close();
    await page.goto('/music/radios');
    await expect(page.getByRole('link', { name, exact: true })).toBeVisible();
    await page.getByRole('link', { name, exact: true }).press('Enter');
    await page.getByText('Edit station', { exact: true }).press('Enter');
    await edit.getByLabel('Who can view', { exact: true }).selectOption('private');
    await submit(page, edit.getByRole('button', { name: 'Save station settings', exact: true }), new URL(stationUrl).pathname);
    const member = 'music_viewer_' + info.project.name.replaceAll('-', '_');
    await page.getByLabel('Local username', { exact: true }).fill(member);
    await submit(page, page.getByRole('button', { name: 'Grant access', exact: true }), new URL(stationUrl).pathname);
    await expect(page.getByRole('button', { name: 'Revoke access', exact: true })).toHaveCount(1);
    const viewerPage = await viewer.newPage();
    await viewerPage.goto('/login');
    await viewerPage.locator('input[name="identifier"]').fill(member);
    await viewerPage.locator('input[name="password"]').fill(process.env.A11Y_PASSWORD);
    await Promise.all([viewerPage.waitForURL(url => url.pathname === '/'), viewerPage.locator('button[type="submit"]').click()]);
    await viewerPage.goto(stationUrl);
    await expect(viewerPage.getByRole('heading', { name, exact: true })).toBeVisible();
    expect(await viewerPage.content()).not.toContain('private_setting');
    await expect(viewerPage.getByText('Edit station', { exact: true })).toHaveCount(0);
    await audit(viewerPage, noScript);
    const saveForm=viewerPage.locator('form.music-saved-form');
    const invalidSave=await viewer.request.post('/web/music/saved',{form:{csrf:'invalid',kind:'radio',id:new URL(stationUrl).pathname.split('/').at(-1),operation:'save'},maxRedirects:0});
    expect(invalidSave.status()).toBe(403);
    await submit(viewerPage,saveForm.getByRole('button',{name:'Save to your library',exact:true}),new URL(stationUrl).pathname);
    await expect(viewerPage.getByRole('button',{name:'Remove from your library',exact:true})).toBeVisible();
    const personalUrl='/music/mine?'+new URLSearchParams({kind:'radios',q:name});
    await viewerPage.goto(personalUrl);
    await expect(viewerPage.getByRole('link',{name,exact:true})).toBeVisible();
    expect(await viewerPage.content()).not.toContain('private_setting');
    await audit(viewerPage,noScript);
    await viewerPage.screenshot({path:info.outputPath('music-my-music-saved-station.png'),fullPage:true});
    await page.getByText('Edit station', { exact: true }).press('Enter');
    await audit(page, noScript);
    await page.screenshot({ path: info.outputPath('music-radio-settings-and-grants.png'), fullPage: true });
    await submit(page, page.getByRole('button', { name: 'Revoke access', exact: true }), new URL(stationUrl).pathname);
    expect((await viewerPage.goto(stationUrl)).status()).toBe(404);
    await viewerPage.goto(personalUrl);
    await expect(viewerPage.getByRole('link',{name,exact:true})).toHaveCount(0);
    expect(await viewerPage.content()).not.toContain('private_setting');
    await viewerPage.goto('/music/saved');
    await expect(viewerPage.getByText('This saved resource is no longer available to you.',{exact:true})).toBeVisible();
    expect(await viewerPage.content()).not.toContain(name);
    await audit(viewerPage,noScript);
    await viewerPage.screenshot({path:info.outputPath('music-saved-unavailable-manager.png'),fullPage:true});
    await submit(viewerPage,viewerPage.getByRole('button',{name:'Remove from your library',exact:true}),'/music/saved');
    await expect(viewerPage.getByText('No saved references.',{exact:true})).toBeVisible();
    await page.getByText('Edit station', { exact: true }).press('Enter');
    await edit.getByLabel('Who can view', { exact: true }).selectOption('local');
    await submit(page, edit.getByRole('button', { name: 'Save station settings', exact: true }), new URL(stationUrl).pathname);
    expect((await anonymous.request.get(stationUrl)).status()).toBe(404);
    expect((await viewerPage.goto(stationUrl)).status()).toBe(200);
    await page.locator('summary').filter({ hasText: /^Delete station$/ }).press('Enter');
    await submit(page, page.getByRole('button', { name: 'Delete station', exact: true }), '/music/radios?mine=true');
    await page.waitForURL(url => url.pathname === '/music/radios' && url.searchParams.get('mine') === 'true');
    expect((await viewer.request.get(stationUrl)).status()).toBe(404);
    await expect(page.getByRole('link', { name, exact: true })).toHaveCount(0);
  } finally {
    await anonymous.close();
    await viewer.close();
  }
}
test('radio station settings and revocable member access are accessible', async ({ page, browser }, info) => journey(page, browser, info, false));
test('radio station settings work without JavaScript @no-js', async ({ page, browser }, info) => journey(page, browser, info, true));
