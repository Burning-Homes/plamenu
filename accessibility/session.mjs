import { expect, test } from '@playwright/test';

// Each worker signs in through the real browser form once. Reuse only its
// session cookie in fresh test contexts: public-page tests remain signed out,
// without creating storage-state files. Do not raise production rate limits
// to accommodate an expanding matrix of tests against one fixture account.
const sessions = new Map();

export async function signIn(page) {
  const key = test.info().project.name;
  const cached = sessions.get(key);
  if (cached) {
    await page.context().addCookies(cached);
    await page.goto('/');
    await expect(page).toHaveURL(url => url.pathname === '/');
  } else {
    await page.goto('/login');
    await page.locator('input[name="identifier"]').fill(process.env.A11Y_USERNAME || 'developer');
    await page.locator('input[name="password"]').fill(process.env.A11Y_PASSWORD || 'plamenu-development-only');
    await Promise.all([
      page.waitForURL(url => url.pathname === '/'),
      page.locator('button[type="submit"]').click(),
    ]);
    const cookies = (await page.context().cookies()).filter(cookie => cookie.name === '__Host-plamenu_session');
    expect(cookies).toHaveLength(1);
    sessions.set(key, cookies);
  }
  await expect(page.locator('a.compose-btn[href="/compose"]').first()).toBeAttached();
}
