import { test, expect } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import { signIn, signInParticipant, participantUsername, openVideoOptions, createVideoChannel ,videoChannelForm} from '../session.mjs';

async function form(page, operation) {
  if (operation === 'profile') return page.locator('form[action^="/web/video-channels/"][action$="/profile"]');
  return page.locator(`form:has(input[name="operation"][value="${operation}"])`);
}
async function submitAndCommit(page, button, channelPath) {
  const posted=page.waitForResponse(response=>response.request().method()==='POST'
    && new URL(response.url()).pathname===channelPath.replace('/videos/channels/','/web/video-channels/'));
  await button.press('Enter');
  expect((await posted).status()).toBe(303);
}
async function scan(page, noScript) {
  const widths = await page.evaluate(() => [document.documentElement.clientWidth, document.documentElement.scrollWidth]);
  expect(widths[1]).toBeLessThanOrEqual(widths[0]);
  if (!noScript) {
    const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
    expect(result.violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) }))).toEqual([]);
  }
}

async function journey(page, browser, info, noScript) {
  test.setTimeout(120_000);
  if (noScript) await page.setViewportSize({width:320,height:800});
  const collaborator = participantUsername(info);
  await signIn(page);
  await page.goto('/videos/upload');await openVideoOptions(page);
  const name = `team_${Date.now().toString(36)}_${info.project.name.slice(0, 3)}`;
  const create = await videoChannelForm(page);
  if (!await create.isVisible()) await create.locator('..').locator(':scope > summary').click();
  await create.getByLabel('Channel address').fill(name);
  await create.getByLabel('Channel name').fill(`Team ${name}`);
  await createVideoChannel(page, create.getByRole('button', { name: 'Create a video channel' }));
  await page.waitForURL(url => url.pathname === '/videos/upload');await openVideoOptions(page);
  // Upload has creation controls; channel management is linked from Videos.
  if (!await page.getByRole('link', {name:'Videos',exact:true}).count()) {
    await page.locator('details[data-drawer] > summary').press('Enter');
  }
  await page.getByRole('link', {name:'Videos',exact:true}).first().press('Enter');
  await page.getByRole('link', {name:'Manage video channels',exact:true}).press('Enter');
  await page.getByRole('link', { name: `Team ${name}`, exact: true }).click();
  const channelPath = new URL(page.url()).pathname;
  await scan(page, noScript);
  let profile = await form(page, 'profile');
  await profile.getByLabel('Channel name').fill(`Team revised ${name}`);
  await profile.getByRole('textbox', { name: 'Description', exact: true }).fill('A team sharing accessible films.');
  await profile.getByRole('button', { name: 'Save channel profile' }).press('Enter');
  await expect(page.getByRole('heading', { level: 1 })).toContainText(`Team revised ${name}`);
  let invite = await form(page, 'invite');
  await invite.getByLabel("Local person's username").fill(collaborator);
  await invite.getByRole('combobox', { name: 'Role', exact: true }).selectOption('editor');
  await invite.getByRole('button', { name: 'Invite collaborator' }).press('Enter');
  await expect(page.getByText('Awaiting acceptance', { exact: false }).first()).toBeVisible();

  const context = await browser.newContext({
    ...info.project.use,
    baseURL: process.env.A11Y_BASE_URL || 'https://plamenu.local',
    javaScriptEnabled: !noScript,
    ...(noScript ? {viewport:{width:320,height:800}} : {}),
  });
  const recipient = await context.newPage();
  try {
    await signInParticipant(recipient);
    await recipient.goto('/videos/channels');
    await recipient.getByRole('link', { name: `Team revised ${name}`, exact: true }).click();
    await expect(await form(recipient, 'profile')).toHaveCount(0);
    await scan(recipient, noScript);
    await (await form(recipient, 'accept')).getByRole('button', { name: 'Accept invitation' }).press('Enter');
    await expect(recipient.getByText('Your role: Editor', { exact: true })).toBeVisible();
    await expect(await form(recipient, 'invite')).toHaveCount(0);
    profile = await form(recipient, 'profile');
    await profile.getByRole('textbox', { name: 'Description', exact: true }).fill('Corrected by a consenting editor.');
    await submitAndCommit(recipient, profile.getByRole('button', { name: 'Save channel profile' }), `${channelPath}/profile`);
    await page.reload();
    await expect((await form(page, 'profile')).getByRole('textbox', { name: 'Description', exact: true })).toHaveValue('Corrected by a consenting editor.');
    await submitAndCommit(page, page.getByRole('button', { name: `Revoke access for @${collaborator}`, exact: true }), channelPath);
    expect((await recipient.request.get(channelPath)).status()).toBe(404);
    await page.locator('summary').filter({ hasText: /^Transfer ownership$/ }).press('Enter');
    let transfer = await form(page, 'transfer');
    await transfer.getByLabel("Local person's username").fill(collaborator);
    await submitAndCommit(page, transfer.getByRole('button', { name: 'Request ownership transfer' }), channelPath);
    await recipient.goto('/videos/channels');
    await recipient.getByRole('link', { name: `Team revised ${name}`, exact: true }).click();
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
