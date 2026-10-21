import { expect, test } from '@playwright/test';
import { uiFixture } from '../ui-fixture.mjs';

for (const prefix of ['', '@no-js ']) {
  test(`${prefix}post menu fits from its first painted frame above mobile navigation`, async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 700 });
    const errors = await uiFixture(page, `<main style="padding:12px;max-width:600px;margin:auto">
      <div style="height:1500px"></div><article class="status"><p>A post near the bottom</p>
      <div class="status__actions"><details class="status__menu" data-status-menu>
      <summary class="action" aria-label="Post options">…</summary><div class="status__menu-pop">
      ${Array.from({ length: 10 }, (_, i) => `<a class="status__menu-item" href="#action-${i}">Action ${i}</a>`).join('')}
      </div></details></div></article><div style="height:1000px"></div></main>
      <nav id="bottom-bar" style="position:fixed;bottom:0;height:56px;left:0;right:0;background:#bbb">Navigation</nav>`);
    for (const y of [80, 330, 550]) {
      await page.locator('summary').evaluate((el, y) => scrollBy(0, el.getBoundingClientRect().top - y), y);
      if (!prefix) await page.evaluate(() => {
        window.menuFrames = [];
        window.samplingMenu = true;
        const sample = () => {
          const menu = document.querySelector('details');
          if (menu.open) {
            const { x, y, width, height, bottom, right } = menu.querySelector('.status__menu-pop').getBoundingClientRect();
            menuFrames.push({ x, y, width, height, bottom, right });
          }
          if (samplingMenu) requestAnimationFrame(sample);
        };
        requestAnimationFrame(sample);
      });
      await page.locator('summary').click();
      await expect(page.locator('.status__menu-pop')).toBeVisible();
      if (!prefix) await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
      const frames = prefix ? [await page.locator('.status__menu-pop').evaluate(el => {
        const { x, y, width, height, bottom, right } = el.getBoundingClientRect();
        return { x, y, width, height, bottom, right };
      })] : await page.evaluate(() => { samplingMenu = false; return menuFrames; });
      expect(frames.length).toBeGreaterThan(0);
      for (const rect of frames) {
        expect(rect).toEqual(frames[0]);
        expect(rect.x).toBeGreaterThanOrEqual(0);
        expect(rect.right).toBeLessThanOrEqual(390);
        expect(rect.y).toBeGreaterThanOrEqual(0);
        expect(rect.bottom).toBeLessThanOrEqual(644);
      }
      // Native summary activation works with scripts disabled as well.
      await page.locator('summary').click();
      await expect(page.locator('details')).not.toHaveAttribute('open');
    }
    expect(errors).toEqual([]);
  });
}
