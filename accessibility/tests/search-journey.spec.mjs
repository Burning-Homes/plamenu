import { test, expect } from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import { signIn } from '../session.mjs';

async function searchJourney(page, info) {
  await signIn(page);
  await page.goto('/search');
  await expect.soft(page.getByRole('heading', { name: 'Search', level: 1, exact: true })).toHaveCount(1, { timeout: 1500 });
  const input = page.locator('#search-q');
  await expect(input).toBeFocused();
  await input.fill('developer');
  await input.press('Enter');
  await expect(page.getByRole('heading', { name: 'People', exact: true })).toBeVisible();
  await expect(input).toHaveValue('developer');
  await page.locator('main .account-list a[href="/@developer"]').first().press('Enter');
  await expect(page).toHaveURL(/\/@developer$/);
  await page.goto('/search');
  const absent = `no_result_${info.project.name}_2026`;
  await input.fill(absent);
  await input.press('Enter');
  await expect(page.locator('main .empty')).toContainText(absent);
  await expect(input).toHaveValue(absent);
  await expect.soft(page.getByRole('heading', { name: 'Search', level: 1, exact: true })).toHaveCount(1, { timeout: 1500 });
  if (info.project.use.javaScriptEnabled !== false) {
    const result = await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
    expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
}

test('search exposes its page heading and supports result and empty-state journeys', async ({page}, info) => searchJourney(page, info));
test('@no-js search exposes its page heading and supports result and empty-state journeys', async ({page}, info) => searchJourney(page, info));
