import { expect, test } from '@playwright/test';
import { uiFixture } from '../ui-fixture.mjs';

test('long composer input keeps the editing line visible without page scroll jumps', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 700 });
  const errors = await uiFixture(page, `<main style="max-width:600px;margin:auto;padding:12px">
    <div style="height:800px"></div><form class="compose-form" data-compose><div class="compose">
    <textarea name="status" rows="5" data-autogrow aria-label="Post text"></textarea>
    <span data-compose-count></span><button type="submit">Publish</button></div></form>
    <div style="height:1200px"></div></main>`);
  const textarea = page.locator('textarea');
  await textarea.fill(Array.from({ length: 90 }, (_, i) => `Row ${i}`).join('\n'));
  await textarea.press('Control+End');
  await textarea.evaluate(el => el.scrollIntoView({ block: 'center' }));
  const samples = async (keys) => {
    const before = await page.evaluate(() => scrollY);
    await page.evaluate(() => {
      window.composeScrollFrames = [];
      window.sampleComposer = true;
      const sample = () => { composeScrollFrames.push(scrollY); if (sampleComposer) requestAnimationFrame(sample); };
      sample();
    });
    await textarea.pressSequentially(keys, { delay: 25 });
    const frames = await page.evaluate(() => { sampleComposer = false; return composeScrollFrames; });
    expect(Math.max(...frames) - Math.min(...frames)).toBeLessThan(60);
    expect(Math.abs(await page.evaluate(() => scrollY) - before)).toBeLessThan(60);
    const state = await textarea.evaluate(el => {
      const rect = el.getBoundingClientRect();
      const lineHeight = parseFloat(getComputedStyle(el).lineHeight);
      const row = el.value.slice(0, el.selectionEnd).split('\n').length - 1;
      const caret = row * lineHeight + parseFloat(getComputedStyle(el).paddingTop) - el.scrollTop;
      return { caret, height: el.clientHeight, top: rect.top, bottom: rect.bottom };
    });
    expect(state.caret).toBeGreaterThanOrEqual(0);
    expect(state.caret).toBeLessThan(state.height);
    expect(state.top + state.caret).toBeGreaterThanOrEqual(0);
    expect(state.top + state.caret).toBeLessThan(page.viewportSize().height - 30);
  };
  await samples(' typing at the end');
  await textarea.press('Control+Home');
  await samples('Beginning ');
  for (let i = 0; i < 45; i++) await textarea.press('ArrowDown');
  await samples(' middle');
  // A reduced visual viewport models the space available above a keyboard.
  await page.setViewportSize({ width: 390, height: 360 });
  await textarea.press('Control+End');
  await textarea.evaluate(el => el.scrollIntoView({ block: 'center' }));
  await samples(' keyboard');
  expect(errors).toEqual([]);
});
