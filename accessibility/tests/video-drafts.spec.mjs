import { expect, test } from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import { signIn } from '../session.mjs';

// These known draft states are seeded in the isolated runner. Upload-to-queue
// and the real encoder are tested separately; this is no claim of that E2E.
async function recovery(page, info, noScript) {
  const project = info.project.name;
  await signIn(page);
  await page.goto('/videos?view=drafts');
  await page.getByRole('link', { name: `Draft ${project} ready`, exact: true }).click();
  const readyUrl = page.url();
  await expect(page.getByRole('status')).toHaveText('Ready to review');
  await expect(page.getByRole('link', { name: 'Refresh status', exact: true })).toBeVisible();
  await page.getByRole('link', { name: 'Review your video', exact: true }).press('Enter');
  const player = page.locator('video');
  await expect(player.locator('source')).toHaveAttribute('src', /\/videos\/drafts\/\d+\/preview\/video$/);
  await expect(page.getByRole('combobox', { name: 'Channel', exact: true })).toBeDisabled();
  await page.locator('summary').filter({ hasText: /^Visual description$/ }).press('Enter');
  await expect(page.getByText('A moving colour test pattern.', { exact: true })).toBeVisible();
  await player.scrollIntoViewIfNeeded();
  await player.evaluate(async video => {
    video.muted = true;
    video.textTracks[0].mode = 'showing';
    await video.play();
  });
  await expect.poll(() => player.evaluate(video => video.currentTime)).toBeGreaterThan(0.1);
  await expect.poll(() => player.evaluate(video => video.textTracks[0].activeCues?.[0]?.text)).toBe('[Steady test tone]');
  expect(await player.evaluate(video => video.error)).toBeNull();
  await expect(page.getByRole('navigation', { name: 'Recent video drafts', exact: true })).toHaveCount(0);
  await checkLayout(page, noScript);
  await page.screenshot({ path: info.outputPath('draft-review.png'), fullPage: true });
  await page.goto(readyUrl);
  await page.locator('summary').filter({hasText:/^Cancel and delete draft$/}).press('Enter');
  const confirm = page.getByRole('checkbox', { name: 'I want to delete this draft and its files', exact: true });
  const cancel = page.getByRole('button', { name: 'Cancel and delete draft', exact: true });
  await expect(page.locator('[data-draft-expiry] time[datetime][data-absolute]')).toBeVisible();
  for (const control of [cancel, page.locator('.video-draft label'), ...await page.locator('.video-draft .settings-form__actions a').all()]) {
    const box = await control.boundingBox();
    expect(box.height).toBeGreaterThanOrEqual(44);
    expect(box.width).toBeGreaterThanOrEqual(44);
  }
  await checkLayout(page, noScript);
  await page.screenshot({ path: info.outputPath('draft-ready.png'), fullPage: true });
  await cancel.press('Enter');
  await expect(page).toHaveURL(readyUrl);
  await expect(confirm).toBeFocused();
  await confirm.press('Space');
  await cancel.press('Enter');
  await page.waitForURL(url => url.pathname === '/videos');
  await expect(page.getByRole('link', { name: `Draft ${project} ready`, exact: true })).toHaveCount(0);

  await page.getByRole('link', { name: `Draft ${project} failed`, exact: true }).click();
  await expect(page.getByRole('status')).toContainText('Your original is kept');
  await expect(page.locator('[data-video-review]')).toBeHidden();
  await checkLayout(page, noScript);
  await page.getByRole('button', { name: 'Retry processing', exact: true }).press('Enter');
  await expect(page.getByRole('status')).toHaveText(/^(Waiting for processing|Processing your video|Ready to review)$/);
  await expect(page.getByRole('button', { name: 'Retry processing', exact: true })).toHaveCount(0);
  await page.getByRole('link', { name: 'Refresh status', exact: true }).press('Enter');
  await expect(page.getByRole('status')).toHaveText(/^(Waiting for processing|Processing your video|Ready to review)$/);
  await page.locator('summary').filter({hasText:/^Cancel and delete draft$/}).press('Enter');
  await page.getByRole('checkbox', { name: 'I want to delete this draft and its files', exact: true }).press('Space');
  await page.getByRole('button', { name: 'Cancel and delete draft', exact: true }).press('Enter');
  await page.waitForURL(url => url.pathname === '/videos');
}

async function checkLayout(page, noScript) {
  const reflow = await page.evaluate(() => ({ width: innerWidth, content: document.documentElement.scrollWidth }));
  expect(reflow.content).toBeLessThanOrEqual(reflow.width);
  if (!noScript) {
    const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
    expect(result.violations.map(v => ({ id: v.id, targets: v.nodes.map(n => n.target) }))).toEqual([]);
  }
}

test('private draft review and keyboard recovery', async ({ page }, info) => recovery(page, info, false));
test('private draft review and keyboard recovery @no-js', async ({ page }, info) => recovery(page, info, true));
