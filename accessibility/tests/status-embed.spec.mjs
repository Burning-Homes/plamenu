import { expect, request, test } from '@playwright/test';
import { signIn } from '../session.mjs';

test('@no-js private posts cannot be embedded through a signed-in frame', async ({ page }) => {
  test.skip(test.info().project.name !== 'chromium-no-javascript');
  await signIn(page);
  await page.goto('/compose');
  const form = page.locator('form[action="/web/compose"]');
  const csrf = await form.locator('input[name="csrf"]').inputValue();
  await form.locator('textarea[name="status"]').fill('Private frame authorization regression.');
  await form.locator('select[name="visibility"]').selectOption('private');
  await form.getByRole('button', { name: 'Publish', exact: true }).click();
  await page.waitForURL(url => /\/@[^/]+\/\d+$/.test(url.pathname));
  const id = page.url().split('/').pop();
  try {
    const anonymous = await request.newContext({ ignoreHTTPSErrors: true });
    try {
      const denied = await anonymous.get(`${new URL(page.url()).origin}/web/statuses/${id}/embed`);
      expect(denied.status()).toBe(404);
      const oembed = await anonymous.get(`${new URL(page.url()).origin}/api/oembed?url=${encodeURIComponent(page.url())}`);
      expect(oembed.status()).toBe(404);
    } finally {
      await anonymous.dispose();
    }
    expect((await page.request.get(`/web/statuses/${id}/embed`)).status()).toBe(404);
  } finally {
    const deleted = await page.request.post(`/web/statuses/${id}/delete`, {
      form: { csrf }, maxRedirects: 0,
    });
    expect(deleted.status()).toBe(303);
  }
});
