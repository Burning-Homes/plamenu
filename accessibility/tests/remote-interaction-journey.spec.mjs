import { test, expect } from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';

async function remoteInteraction(page, info) {
  await page.goto('/@developer');
  await page.locator('a[href^="/interact?uri="]').first().press('Enter');
  const form = page.locator('form[action="/interact"]');
  const subject = await form.locator('[name=uri]').inputValue();
  await form.locator('[name=handle]').fill('not-a-federated-handle');
  await form.locator('button[type=submit]').press('Enter');
  const error = page.locator('#interact-errors');
  await expect(error).toBeVisible();
  await page.screenshot({ path: info.outputPath('remote-interaction-error.png'), fullPage: true });
  await expect.soft(error).toBeFocused({ timeout: 1500 });
  await expect.soft(error.getByRole('list')).toHaveCount(1, { timeout: 1500 });
  await page.keyboard.press('Tab');
  await expect(form.locator('[name=handle]')).toBeFocused();
  await expect(form.locator('[name=uri]')).toHaveValue(subject);
  await expect(form.locator('[name=handle]')).toHaveValue('not-a-federated-handle');
  // Axe needs JavaScript; the no-JS journey keeps native form/focus assertions.
  if (info.project.use.javaScriptEnabled !== false) {
    const result = await new AxeBuilder({ page }).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
    expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
  // The local-home branch needs no outbound request and leads to existing login.
  await form.locator('[name=handle]').fill('developer@localhost');
  await form.locator('button[type=submit]').press('Enter');
  await expect(page).toHaveURL(/\/login$/);
  await expect(page.getByRole('heading', { name: 'Sign in', exact: true })).toBeVisible();
}

test('remote-follow interstitial retains context and exposes handle errors', async ({page}, info) => remoteInteraction(page, info));
test('@no-js remote-follow interstitial retains context and exposes handle errors', async ({page}, info) => remoteInteraction(page, info));
