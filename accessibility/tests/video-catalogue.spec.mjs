import { expect, test } from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import { signIn } from '../session.mjs';

// Isolated 5,550-row authority fixture. Decoder/pipeline coverage lives in
// native-video and video-distribution, not these poster-only navigation tests.
async function catalogue(page, info, noScript) {
  await signIn(page);
  await page.setViewportSize({ width: 320, height: 800 });
  await page.goto('/videos/explore');
  await page.getByRole('searchbox', { name: 'Search video titles and descriptions' }).fill('Catalogue boundary');
  await page.getByRole('button', { name: 'Search videos' }).press('Enter');
  const cards=page.locator('article.video-explore__card');
  await expect(cards).toHaveCount(0);
  await expect(page.getByText('No visible videos on this page. Continue browsing for older matches.', { exact: true })).toBeVisible();
  await expect(page.locator('body')).not.toContainText('Catalogue boundary hidden fixture');
  const older=page.getByRole('link', { name: 'Show more', exact: true });
  await expect(older).toBeVisible();
  const firstCursor=new URL(await older.getAttribute('href'),page.url()).searchParams.get('max_id');
  expect(Number(firstCursor)).toBe(1001551);
  await layout(page,noScript);
  await page.screenshot({ path: info.outputPath('filtered-empty-320px.png'), fullPage: true });
  await older.press('Enter');
  // Enhanced galleries prefetch one page; native HTML keeps 20 per page.
  await expect(cards).toHaveCount(noScript ? 20 : 40);
  await expect(cards.first()).toContainText('Catalogue boundary visible fixture');
  await expect(page.locator('.video-explore video')).toHaveCount(0);
  await expect(page.getByRole('searchbox')).toHaveValue('Catalogue boundary');
  const visited=new Set();
  // Automatic prefetch supplies 40 cards. The ordinary pager is a native
  // navigation, so retain those identities before keyboard-opening the final
  // ten. This also verifies the fallback rather than assuming an append.
  for (const expected of noScript ? [20,20,10] : [40,10]) {
    await expect(cards).toHaveCount(expected);
    await expect(page.locator('body')).not.toContainText('Catalogue boundary hidden fixture');
    for (const href of await cards.locator('a.video-explore__watch').evaluateAll(links=>links.map(a=>a.getAttribute('href')))) {
      expect(visited.has(href)).toBe(false);
      visited.add(href);
    }
    await layout(page,noScript);
    if(expected===10) await expect(older).toHaveCount(0);
    else await older.press('Enter');
  }
  expect(visited.size).toBe(50);
  await page.screenshot({ path: info.outputPath('filtered-last-page-320px.png'), fullPage: true });
}
async function layout(page,noScript) {
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
  if(!noScript) {
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
    expect(result.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
}
test('filtered catalogue keeps bounded keyboard navigation to all visible results',async({page},info)=>catalogue(page,info,false));
test('filtered catalogue keeps bounded keyboard navigation to all visible results @no-js',async({page},info)=>catalogue(page,info,true));
