import { test, expect } from '@playwright/test';
import { signIn } from '../session.mjs';

for (const prefix of ['', '@no-js ']) {
  test(`${prefix}publishing in a long thread scrolls to the new reply`, async ({ page }) => {
    await signIn(page);
    await page.goto('/compose');
    await page.locator('textarea[name=status]').fill('Long thread root\n' + 'A line in the earlier conversation.\n'.repeat(45));
    await page.locator('.compose__publish').click();
    await expect(page).toHaveURL(/\/@[^/]+\/\d+/);
    for (const dedicated of [false, true]) {
      const previous = new URL(page.url()).pathname.split('/').at(-1);
      if (dedicated) await page.goto(`/compose?reply=${previous}`);
      await page.locator('textarea[name=status]').fill('Published answer ' + Date.now());
      await page.locator('.compose__publish').click();
      await expect(page).toHaveURL(url => url.hash === `#post-${url.pathname.split('/').at(-1)}`);
      const focus = page.locator('.thread__focus');
      await expect(focus).toContainText('Published answer');
      const box = await focus.boundingBox();
      expect(box.y).toBeGreaterThanOrEqual(-1);
      expect(box.y).toBeLessThan(page.viewportSize().height / 2);
    }
  });
}
