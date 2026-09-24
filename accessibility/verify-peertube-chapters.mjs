// Real first-party chapter links: baseline no-JS and enhanced keyboard seek.
import { chromium } from 'playwright';
import AxeBuilder from '@axe-core/playwright';

const [watchUrl, expectedCountRaw, seekSecondsRaw, screenshotPath] = process.argv.slice(2);
if (!watchUrl) throw new Error('expected watch URL');
const expectedCount = Number(expectedCountRaw);
const seekSeconds = Number(seekSecondsRaw);
const browser = await chromium.launch();
try {
  const noJs = await browser.newContext({ ignoreHTTPSErrors: true, javaScriptEnabled: false });
  const staticPage = await noJs.newPage();
  await staticPage.goto(watchUrl);
  const links = await staticPage.locator('.media__chapters a[data-chapter-seek]').evaluateAll(nodes =>
    nodes.map(node => ({ text: node.textContent.trim(), href: node.href, start: Number(node.dataset.chapterSeek) })));
  if (links.length !== expectedCount || !links.some(link => link.href.endsWith(`#t=${seekSeconds}`))) {
    throw new Error(`no-JS chapter links missing: ${JSON.stringify(links)}`);
  }
  if (links.some(link => !link.href.startsWith('https://plamenu.local/media/'))) {
    throw new Error(`chapter link leaked remote media: ${JSON.stringify(links)}`);
  }
  await noJs.close();

  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.goto(watchUrl);
  const canonicalUrl = page.url();
  const chapters = page.locator('.media__chapters');
  await chapters.locator('summary').click();
  const audit = await new AxeBuilder({ page }).include('.media__chapters').analyze();
  const severe = audit.violations.filter(item => ['critical', 'serious'].includes(item.impact));
  if (severe.length) throw new Error(`chapter accessibility: ${JSON.stringify(severe)}`);
  if (screenshotPath) {
    await chapters.scrollIntoViewIfNeeded();
    await page.screenshot({ path: screenshotPath, fullPage: true });
  }
  const link = chapters.locator(`a[data-chapter-seek="${seekSeconds}"]`);
  await link.focus();
  await page.keyboard.press('Enter');
  await page.waitForFunction(seconds => document.querySelector('figure.media--video video')?.currentTime >= seconds - 0.5,
    seekSeconds, { timeout: 45000 });
  const time = await page.locator('figure.media--video video').evaluate(video => video.currentTime);
  if (new URL(page.url()).pathname !== new URL(canonicalUrl).pathname) {
    throw new Error(`enhanced seek navigated away: ${JSON.stringify({url: page.url(), canonicalUrl, errors})}`);
  }
  await page.setViewportSize({ width: 390, height: 844 });
  const mobileOverflow = await page.evaluate(() => document.documentElement.scrollWidth > innerWidth + 1);
  const mobileAudit = await new AxeBuilder({ page }).include('.media__chapters').analyze();
  const mobileSevere = mobileAudit.violations.filter(item => ['critical', 'serious'].includes(item.impact));
  if (mobileOverflow || mobileSevere.length || errors.length) {
    throw new Error(JSON.stringify({ mobileOverflow, mobileSevere, errors }));
  }
  if (screenshotPath?.endsWith('.png')) {
    await page.screenshot({ path: screenshotPath.replace(/\.png$/, '-mobile.png'), fullPage: true });
  }
  await page.emulateMedia({ colorScheme: 'dark' });
  await page.waitForTimeout(600);
  const darkAudit = await new AxeBuilder({ page }).include('.media__chapters').analyze();
  const darkSevere = darkAudit.violations.filter(item => ['critical', 'serious'].includes(item.impact));
  if (darkSevere.length) throw new Error(`dark chapter accessibility: ${JSON.stringify(darkSevere)}`);
  if (screenshotPath?.endsWith('.png')) {
    await page.screenshot({ path: screenshotPath.replace(/\.png$/, '-mobile-dark.png'), fullPage: true });
  }
  console.log(JSON.stringify({ noJsLinks: links, seekTime: time, severe: severe.length,
    mobileSevere: mobileSevere.length, darkSevere: darkSevere.length, mobileOverflow, errors }));
  await context.close();
} finally {
  await browser.close();
}
