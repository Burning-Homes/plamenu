import { expect, test } from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import { randomUUID } from 'node:crypto';
import { signIn, createVideoChannel, videoChannelForm, openDetailsFor } from '../session.mjs';

async function channelJourney(page, noScript = false) {
  await signIn(page);
  await page.goto('/videos/channels/new');
  const username = `actors_${randomUUID().replaceAll('-', '').slice(0, 20)}`;
  const form = await videoChannelForm(page);
  await form.getByLabel('Channel address').fill(username);
  await form.getByLabel('Channel name').fill('A long channel name for compact profile controls');
  await createVideoChannel(page, form.getByRole('button', { name: 'Create a video channel' }));
  await page.goto(`/@${username}`);
  const managePath = await page.locator('.actor-connections__manage a[href^="/videos/channels/"]').first().getAttribute('href');
  const channel = managePath.split('/').at(-1);
  try {
    await page.goto(`/@${username}`);
    await page.locator('.actor-connections__people summary').press('Enter');
    const owner = await page.locator('.actor-connections__people a').first().getAttribute('href');
    await expect(page.locator('.actor-connections__people')).toContainText('Owner');
    await page.locator('.actor-connections__manage summary').press('Enter');
    const manage = page.locator('.actor-connections__manage');
    await expect(manage.getByRole('link', { name: 'Follow requests', exact: true })).toHaveAttribute('href', `/videos/channels/${channel}/requests`);
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth > innerWidth + 1);
    expect(overflow).toBe(false);
    // Axe injects and runs scripts; the no-JS project instead exercises all
    // native disclosures, links and forms with page scripting disabled.
    if (!noScript) expect((await new AxeBuilder({ page }).include('.actor-connections').analyze()).violations).toEqual([]);
    await manage.getByRole('link', { name: 'Upload video', exact: true }).press('Enter');
    await expect(page.locator('select[name="channel"]')).toHaveValue(channel);
    await page.goto(`/videos?channel=${channel}`);
    await expect(page.locator('input[name="channel"]')).toHaveValue(channel);
    await page.goto(owner);
    await page.locator('.actor-connections__people summary').press('Enter');
    await expect(page.locator(`.actor-connections a[href="/@${username}"]`)).toBeVisible();
  } finally {
    await page.goto(`/videos/channels/${channel}`);
    const deletion = page.locator('form').filter({ has: page.locator('input[name="operation"][value="delete"]') });
    await openDetailsFor(deletion);
    await deletion.locator('input[name="confirm"]').fill(username);
    await deletion.getByRole('button', { name: 'Delete channel', exact: true }).press('Enter');
    await expect(page).toHaveURL(url => url.pathname === '/videos/channels');
  }
}

test('channel profiles expose compact role controls and linked ownership', async ({ page }) => channelJourney(page));
test('channel profiles and management work without JavaScript @no-js', async ({ page }) => channelJourney(page, true));
