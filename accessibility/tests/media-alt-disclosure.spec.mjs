import { expect, test } from '@playwright/test';
import { uiFixture } from '../ui-fixture.mjs';

const description = 'A blue and golden bird on a branch. '.repeat(70);
const body = `<main style="padding:12px;max-width:600px;margin:auto"><article class="status">
  <div class="status__media" data-count="4">
  ${Array.from({ length: 4 }, (_, i) => `<figure class="media"><a class="media__link" href="/image-${i}.png">Image ${i}</a>
    <div class="media__badges"><span class="media__alt"><button class="media__badge" type="button" popovertarget="alt-${i}">ALT</button>
    <span class="media__alt-text" id="alt-${i}" popover role="note">${description}</span></span></div></figure>`).join('')}
  </div></article></main>`;

for (const prefix of ['', '@no-js ']) {
  test(`${prefix}ALT toggles compact descriptions outside the image clipping box`, async ({ page }) => {
    await page.setViewportSize({ width: 320, height: 640 });
    const errors = await uiFixture(page, body);
    const button = page.locator('.media__badge').nth(3);
    const panel = page.locator('#alt-3');
    const badge = await button.boundingBox();
    expect(badge.width).toBeLessThan(44);
    expect(badge.height).toBeLessThan(44);
    expect(badge.width).toBeGreaterThanOrEqual(24);
    expect(badge.height).toBeGreaterThanOrEqual(24);
    await button.click();
    await expect(panel).toBeVisible();
    await expect(panel).toHaveText(description.trim());
    const rect = await panel.boundingBox();
    expect(rect.x).toBeGreaterThanOrEqual(0);
    expect(rect.x + rect.width).toBeLessThanOrEqual(320);
    expect(rect.y).toBeGreaterThanOrEqual(0);
    expect(rect.y + rect.height).toBeLessThanOrEqual(584);
    const painted = await panel.evaluate(el => {
      const r = el.getBoundingClientRect();
      return el.contains(document.elementFromPoint(r.right - 15, r.bottom - 15));
    });
    expect(painted).toBe(true);
    // Toggle via the native invoker even if the centered panel covers its
    // original pointer location; keyboard activation is available without JS.
    await button.focus();
    await button.press('Enter');
    await expect(panel).toBeHidden();
    expect(errors).toEqual([]);
  });
}

test('ALT text closes on a tap but survives drag, double click and long press selection', async ({ page }) => {
  const errors = await uiFixture(page, body);
  const button = page.locator('.media__badge').first();
  const panel = page.locator('#alt-0');
  await button.click();
  await panel.click();
  await expect(panel).toBeHidden();
  await button.click();
  const box = await panel.boundingBox();
  await page.mouse.move(box.x + 15, box.y + 20);
  await page.mouse.down();
  await page.mouse.move(box.x + Math.min(160, box.width - 20), box.y + 20, { steps: 12 });
  await page.mouse.up();
  expect(await page.evaluate(() => getSelection().toString().length)).toBeGreaterThan(0);
  await page.waitForTimeout(750); // Longer than the single-tap disambiguation window.
  await expect(panel).toBeVisible();
  await page.evaluate(() => getSelection().removeAllRanges());
  await panel.dblclick({ position: { x: 25, y: 20 } });
  await page.waitForTimeout(750);
  await expect(panel).toBeVisible();
  expect(await page.evaluate(() => getSelection().toString().length)).toBeGreaterThan(0);
  await page.evaluate(() => getSelection().removeAllRanges());
  // A recognized double-click may arrive slower than a quick two-tap gesture.
  await page.mouse.click(box.x + 25, box.y + 20);
  await page.waitForTimeout(400);
  await expect(panel).toBeVisible();
  await page.mouse.click(box.x + 25, box.y + 20, { clickCount: 2 });
  await page.waitForTimeout(750);
  await expect(panel).toBeVisible();
  expect(await page.evaluate(() => getSelection().toString().length)).toBeGreaterThan(0);
  await page.evaluate(() => getSelection().removeAllRanges());
  await panel.click({ position: { x: 25, y: 20 }, delay: 600 });
  await expect(panel).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(panel).toBeHidden();
  expect(errors).toEqual([]);
});
