import { test, expect } from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import { randomUUID } from 'node:crypto';
import { signIn, openVideoOptions, createVideoChannel ,videoChannelForm} from '../session.mjs';

async function submit(page, button, path) {
  const [response] = await Promise.all([
    page.waitForResponse(r => new URL(r.url()).pathname === path && r.request().method() === 'POST'),
    button.press('Enter'),
  ]);
  expect(response.status()).toBe(303);
  await page.waitForLoadState('domcontentloaded');
}
async function check(page, noScript) {
  await page.setViewportSize({ width: 320, height: 800 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  if (!noScript) {
    const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
    expect(result.violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) }))).toEqual([]);
  }
}
async function journey(page, info, noScript) {
  test.setTimeout(240000);
  await signIn(page);
  await page.goto('/videos/upload');await openVideoOptions(page);
  const create = await videoChannelForm(page);
  if (!await create.isVisible()) await create.locator('..').locator(':scope > summary').press('Enter');
  const name = `stats_${randomUUID().replaceAll('-', '').slice(0, 18)}`;
  await create.getByLabel('Channel address').fill(name);
  await create.getByLabel('Channel name').fill(name);
  await submit(page, create.getByRole('button', { name: 'Create a video channel', exact: true }), '/web/video-channels');
  await page.goto('/videos/upload');await openVideoOptions(page);
  const upload = page.locator('form[action="/web/videos"]');
  await upload.getByRole('combobox', { name: 'Channel', exact: true }).selectOption({ label: name });
  const channel = await upload.getByRole('combobox', { name: 'Channel', exact: true }).inputValue();
  await upload.getByLabel('Video title', { exact: true }).fill(`Analytics ${name}`);
  await upload.getByLabel('Visual description', { exact: true }).fill('Moving pattern');
  await upload.getByLabel('Transcript', { exact: true }).fill('Steady tone');
  await upload.getByLabel('Video or audio file', { exact: true }).setInputFiles(process.env.A11Y_VIDEO_FIXTURE);
  await upload.getByRole('button', { name: 'Upload video', exact: true }).press('Enter');
  await page.waitForURL(url => /\/videos\/drafts\/\d+$/.test(url.pathname));
  const media = new URL(page.url()).pathname.split('/').pop();
  if (noScript) {
    for (let i = 0; i < 60; i++) {
      if (await page.getByRole('link', { name: 'Review your video', exact: true }).isVisible()) break;
      await page.waitForTimeout(1000);
      await page.getByRole('link', { name: 'Refresh status', exact: true }).press('Enter');
    }
  }
  await expect(page.getByRole('link', { name: 'Review your video', exact: true })).toBeVisible({ timeout: 60000 });
  await page.getByRole('link', { name: 'Review your video', exact: true }).press('Enter');
  await page.getByRole('button', { name: 'Publish video', exact: true }).press('Enter');
  await page.waitForURL(url => /\/@[^/]+\/\d+/.test(url.pathname));
  await page.goto(`/videos/channels/${channel}`);
  await page.getByRole('link', { name: 'Delivery analytics', exact: true }).press('Enter');
  const enabled = page.getByRole('checkbox', { name: 'Collect anonymous daily delivery totals for this channel', exact: true });
  await expect(enabled).not.toBeChecked();
  await expect(page.getByText('No retained delivery totals.', { exact: false })).toBeVisible();
  const ranged = () => page.request.get(`/media/${media}/download`, { headers: { Range: 'bytes=0-31' } });
  expect((await ranged()).status()).toBe(206);
  const api = `/api/v1/video_channels/${channel}/analytics`;
  // The client API intentionally does not authenticate ambient cookies.
  // Exercise its explicit bearer contract without changing product auth.
  const session = (await page.context().cookies()).find(cookie => cookie.name === '__Host-plamenu_session');
  const read = async () => {
    const response = await page.request.get(api, { headers: { Authorization: `Bearer ${session.value}` } });
    expect(response.status()).toBe(200);
    expect(response.headers()['cache-control']).toBe('private, no-store');
    return response.json();
  };
  expect((await read()).daily).toEqual([]);
  await enabled.press('Space');
  await page.getByLabel('Retention in UTC days (1–90)', { exact: true }).fill('7');
  const savePath = `/web/video-channels/${channel}/analytics`;
  await submit(page, page.getByRole('button', { name: 'Save analytics choices', exact: true }), savePath);
  await expect(enabled).toBeChecked();
  const delivery = await ranged();
  expect(delivery.status()).toBe(206);
  expect((await delivery.body()).length).toBe(32);
  await expect.poll(async () => {
    return (await read()).daily.reduce((sum, row) => sum + row.body_bytes, 0);
  }, { timeout: 45000 }).toBeGreaterThanOrEqual(32);
  await page.reload();
  await expect(page.getByRole('link', { name: `Analytics ${name}`, exact: true })).toBeVisible();
  await expect(page.getByText('Emitted body bytes', { exact: true })).toBeVisible();
  for (const choice of await page.locator('.video-analytics .settings-field__choice').all()) {
    const box = await choice.boundingBox();
    expect(Math.round(box.height * 100) / 100).toBeGreaterThanOrEqual(44);
  }
  await check(page, noScript);
  await page.screenshot({ path: info.outputPath('delivery-analytics-320px.png'), fullPage: true });
  await page.getByRole('checkbox', { name: "Confirm permanent deletion of this channel's delivery totals", exact: true }).press('Space');
  await submit(page, page.getByRole('button', { name: 'Erase delivery totals', exact: true }), savePath);
  await expect(enabled).toBeChecked();
  expect((await read()).daily).toEqual([]);
  await enabled.press('Space');
  await submit(page, page.getByRole('button', { name: 'Save analytics choices', exact: true }), savePath);
  await expect(enabled).not.toBeChecked();
  await check(page, noScript);
}
test('private delivery analytics opt-in, UTC retention and erasure are keyboard accessible', async ({ page }, info) => journey(page, info, false));
test('private delivery analytics opt-in, UTC retention and erasure are keyboard accessible @no-js', async ({ page }, info) => journey(page, info, true));
