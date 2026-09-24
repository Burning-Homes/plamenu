// Browser half of the real PeerTube -> Plamenu schedule E2E. The Python
// federation test creates the remote live, then hands us its public permalink
// while the two announced dates still exist.
import { chromium } from 'playwright';
import AxeBuilder from '@axe-core/playwright';

const [url, screenshot] = process.argv.slice(2);
if (!url || !screenshot) throw new Error('expected public live URL and screenshot path');

const browser = await chromium.launch();
try {
  const noJS = await browser.newContext({
    viewport: { width: 320, height: 800 },
    javaScriptEnabled: false,
    ignoreHTTPSErrors: true,
  });
  const mobile = await noJS.newPage();
  await mobile.goto(url);
  const disclosure = mobile.locator('figure.media--live-offline details');
  await disclosure.locator('summary').click();
  if (await disclosure.locator('time[datetime]').count() !== 2) {
    throw new Error('no-JS mobile schedule disclosure did not expose both dates');
  }
  if (!await mobile.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)) {
    throw new Error('no-JS mobile scheduled live overflows the 320px viewport');
  }
  await mobile.screenshot({ path: screenshot, fullPage: true });
  await noJS.close();

  const desktop = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await desktop.newPage();
  await page.goto(url);
  await page.locator('figure.media--live-offline details summary').click();
  const violations = (await new AxeBuilder({ page }).include('figure.media--live-offline')
    .withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze()).violations;
  if (violations.length) {
    throw new Error(`scheduled live accessibility violations: ${violations.map(v => v.id).join(', ')}`);
  }
  await desktop.close();
} finally {
  await browser.close();
}
