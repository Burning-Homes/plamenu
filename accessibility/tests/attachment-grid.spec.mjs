import { readFileSync } from 'node:fs';
import { test, expect } from '../fixtures.mjs';

const stylesheet = readFileSync(new URL('../../crates/server/src/web/assets/app.css', import.meta.url), 'utf8');

for (const prefix of ['', '@no-js ']) {
  test(`${prefix}attachment grids keep an incomplete final row at the start`, async ({ page }) => {
    for (const count of [3, 5]) {
      await page.setContent(`<meta name="viewport" content="width=device-width,initial-scale=1"><style>${stylesheet}</style><main style="max-width:600px;margin:auto">
        <div class="status__media" data-count="${Math.min(count, 4)}">
        ${Array.from({ length: count }, (_, index) => `<figure class="media"><a class="media__link" href="/image-${index}.png">Image ${index + 1}</a></figure>`).join('')}
        </div></main>`);
      const boxes = await page.locator('figure').evaluateAll(nodes => nodes.map(node => {
        const { x, y, width, height } = node.getBoundingClientRect();
        return { x, y, width, height };
      }));
      expect(boxes[1].y).toBe(boxes[0].y);
      expect(boxes[1].x).toBeGreaterThan(boxes[0].x);
      expect(boxes.at(-1).x).toBe(boxes[0].x);
      expect(boxes.at(-1).y).toBeGreaterThan(boxes[0].y);
    }
  });
}
