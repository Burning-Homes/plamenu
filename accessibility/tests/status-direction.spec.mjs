import { readFileSync } from 'node:fs';
import { expect, test } from '../fixtures.mjs';

const stylesheet = readFileSync(new URL('../../crates/server/src/web/assets/app.css', import.meta.url), 'utf8');
const rtl = 'كيف نقدر نوسّع مساحة الترجمة؟';

// Native direction is tested with the production stylesheet and representative
// card structure. Renderer tests separately cover the emitted attributes.
async function directionAndLayout(page) {
  await page.setContent(`<!doctype html><html lang="en"><head><meta name="viewport" content="width=device-width, initial-scale=1"><style>${stylesheet}</style></head>
    <body><main style="max-width:600px;margin:auto;padding:12px">
      <article class="status">
        <header class="status__head"><span>English author</span><time>3d</time></header>
        <div id="english" class="status__content" dir="auto" lang="ar"><p>English post text.</p></div>
        <div class="quote-card">
          <a class="quote-card__head"><span class="quote-card__name">Quoted author</span><span class="quote-card__acct">@author@example.org</span></a>
          <div id="arabic" class="status__content" dir="auto" lang="en"><p>${rtl}</p></div>
        </div>
        <div id="hebrew" class="status__content" dir="auto"><p>שלום עולם</p></div>
        <div id="mixed" class="status__content" dir="auto"><p>😊 123 ${rtl} <a href="#">English link</a></p></div>
        <details class="status__cw" open><summary dir="auto">تنبيه</summary>
          <div class="status__content" dir="auto"><p>${rtl}</p><pre tabindex="0"><code>${'long_code(); '.repeat(100)}</code></pre>
          <img alt="" width="1200" height="100" src="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='1200' height='100'%3E%3C/svg%3E"></div>
        </details>
        <div class="poll"><div class="poll__result"><div class="poll__bar" style="--pct:60%"></div><span class="poll__title" dir="auto">نعم</span><span class="poll__pct">60%</span></div></div>
        <footer class="status__actions"><button class="action">Reply</button><button class="action">Boost</button><button class="action">Favourite</button></footer>
      </article>
    </main></body></html>`);
  const result = await page.evaluate(() => {
    const direction = selector => getComputedStyle(document.querySelector(selector)).direction;
    const aligned = (id, edge) => {
      const paragraph = document.querySelector(`#${id} p`);
      const range = document.createRange();
      range.selectNodeContents(paragraph);
      return Math.abs(range.getBoundingClientRect()[edge] - paragraph.getBoundingClientRect()[edge]);
    };
    const rect = selector => document.querySelector(selector).getBoundingClientRect();
    return {
      directions: ['#english', '#arabic', '#hebrew', '#mixed', 'summary', '.poll__title'].map(direction),
      chrome: ['.status', '.status__head', '.quote-card__head', '.status__actions', '.poll__result'].map(direction),
      alignment: [aligned('english', 'left'), aligned('arabic', 'right'), aligned('hebrew', 'right')],
      overflow: document.documentElement.scrollWidth - innerWidth,
      imageFits: rect('img').width <= rect('details .status__content').width,
      codeFits: rect('pre').width <= rect('details .status__content').width,
      codeScrolls: document.querySelector('pre').scrollWidth > document.querySelector('pre').clientWidth,
      pollOrder: rect('.poll__title').left < rect('.poll__pct').left,
      actions: [...document.querySelectorAll('.status__actions button')].map(button => button.getBoundingClientRect().left),
    };
  });
  expect(result.directions).toEqual(['ltr', 'rtl', 'rtl', 'rtl', 'rtl', 'rtl']);
  expect(result.chrome).toEqual(['ltr', 'ltr', 'ltr', 'ltr', 'ltr']);
  for (const difference of result.alignment) expect(difference).toBeLessThan(2);
  expect(result.overflow).toBeLessThanOrEqual(1);
  expect(result.imageFits).toBe(true);
  expect(result.codeFits).toBe(true);
  expect(result.codeScrolls).toBe(true);
  expect(result.pollOrder).toBe(true);
  expect(result.actions[0]).toBeLessThan(result.actions[1]);
  expect(result.actions[1]).toBeLessThan(result.actions[2]);

  // Translation and "show original" replace text inside the same container.
  // Automatic direction must update without a script guessing the language.
  await page.locator('#arabic').evaluate(element => { element.innerHTML = '<p>Translated English.</p>'; });
  expect(await page.locator('#arabic').evaluate(element => getComputedStyle(element).direction)).toBe('ltr');
  await page.locator('#arabic').evaluate((element, text) => { element.textContent = text; }, rtl);
  expect(await page.locator('#arabic').evaluate(element => getComputedStyle(element).direction)).toBe('rtl');
}

for (const width of [320, 390, 768, 1440]) {
  test(`status text direction preserves card layout at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 });
    await directionAndLayout(page);
  });
}

test('@no-js status text direction works without page scripts', async ({ page }) => {
  await page.setViewportSize({ width: 320, height: 900 });
  await directionAndLayout(page);
});
