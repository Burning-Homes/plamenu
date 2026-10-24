import { expect, test } from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import { signIn, chooseVideoLanguage, openDetailsFor, ensureVideoFormChannel } from '../session.mjs';

// An explicitly selected recorded fixture on the local PeerTube instance.
// Its bytes really pass through the server importer and native FFmpeg worker.
const source = process.env.A11Y_IMPORT_SOURCE_URL;

async function check(page, noScript) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  if (!noScript) {
    const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
    expect(result.violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) }))).toEqual([]);
  }
}

async function journey(page, info, noScript) {
  test.setTimeout(180_000);
  expect(source, 'set A11Y_IMPORT_SOURCE_URL to a recorded local PeerTube AP Video fixture').toBeTruthy();
  await signIn(page);
  await page.goto('/videos');
  await page.locator('.video-workspace__more > summary').press('Enter');
  await page.getByRole('link', { name: 'Import a video', exact: true }).press('Enter');
  await expect(page.getByRole('heading', { name: 'Import a video', exact: true })).toBeVisible();
  await expect(page.getByText('This is a one-time copy', { exact: false })).toBeVisible();
  const form = page.locator('form[action="/web/videos/import"]');
  await ensureVideoFormChannel(page, form);
  const title = `Imported browser ${info.project.name}`;
  await form.getByLabel('Video title', { exact: true }).fill(title);
  const description = form.getByLabel('Description', { exact: true });
  await openDetailsFor(description);
  await expect(description).toBeVisible();
  await description.fill('Copied with permission from the local test PeerTube instance.');
  await form.getByLabel('Visual description', { exact: true }).fill('An animated test video.');
  await form.getByLabel('Transcript', { exact: true }).fill('A steady test tone.');
  await chooseVideoLanguage(form,'en');
  await form.getByRole('combobox', { name: 'Source format', exact: true }).selectOption('activitypub');
  await form.getByLabel('Public source URL', { exact: true }).fill('https://username:secret@example.org/video.mp4');
  const submit = form.getByRole('button', { name: 'Import into a private draft', exact: true });
  const rights = form.getByRole('checkbox', { name: 'I have permission to copy and publish this video, and will review its licence and accessibility', exact: true });
  await submit.press('Enter');
  await expect(rights).toBeFocused();
  await rights.press('Space');
  await check(page, noScript);
  await submit.press('Enter');
  await expect(page.getByRole('alert')).toHaveText('Use a URL without credentials or a fragment');
  await expect(form.getByLabel('Video title', { exact: true })).toHaveValue(title);
  await form.getByLabel('Public source URL', { exact: true }).fill(source);
  await rights.press('Space');
  await page.screenshot({ path: info.outputPath('source-import-form.png'), fullPage: true });
  await submit.press('Enter');
  await page.waitForURL(url => /\/videos\/drafts\/\d+$/.test(url.pathname));
  await expect(page.getByText('Only you can access this draft. It has not been published.', { exact: true })).toBeVisible();
  const draftUrl = page.url();
  if (noScript) {
    for (let attempt = 0; attempt < 60; attempt++) {
      const text = await page.getByRole('status').innerText();
      if (text === 'Ready to review') break;
      expect(text).not.toMatch(/could not be downloaded|could not be processed/);
      // A scripts-disabled page deliberately does not live-update. Simulate
      // a person refreshing periodically, not sixty requests in one second.
      await page.waitForTimeout(1000);
      await page.getByRole('link', { name: 'Refresh status', exact: true }).press('Enter');
    }
  }
  await expect(page.getByRole('status')).toHaveText('Ready to review', { timeout: 120_000 });
  await page.getByRole('link', { name: 'Review your video', exact: true }).press('Enter');
  await expect(page.locator('details').filter({ has: page.locator('summary').filter({ hasText: /^Visual description$/ }) })).toContainText('An animated test video.');
  await expect(page.locator('form[action="/web/videos"]')).toContainText('A steady test tone.');
  const video = page.locator('video.video-publish__preview');
  await video.evaluate(async video => { video.muted = true; await video.play(); });
  await expect.poll(() => video.evaluate(video => video.currentTime), { timeout: 30_000 }).toBeGreaterThan(0.1);
  await expect.poll(() => video.evaluate(video => video.videoWidth)).toBeGreaterThan(0);
  await video.evaluate(video => video.pause());
  expect(await page.locator('script[src*="p2p-media-loader"]').count()).toBe(0);
  await check(page, noScript);
  await page.screenshot({ path: info.outputPath('source-import-review.png'), fullPage: true });
  // Review must not publish. Leave no large source files behind in this fixture.
  await page.goto(draftUrl);
  const deleteConfirmation = page.getByRole('checkbox', { name: 'I want to delete this draft and its files', exact: true, includeHidden: true });
  await openDetailsFor(deleteConfirmation);
  await expect(deleteConfirmation).toBeVisible();
  await deleteConfirmation.press('Space');
  await page.getByRole('button', { name: 'Cancel and delete draft', exact: true }).press('Enter');
  await page.waitForURL(url => url.pathname === '/videos');
  await expect(page.getByRole('link', { name: title, exact: true })).toHaveCount(0);
}

test('native source import has accessible validation and decoded private review', async ({ page }, info) => journey(page, info, false));
test('native source import has accessible validation and decoded private review @no-js', async ({ page }, info) => journey(page, info, true));
