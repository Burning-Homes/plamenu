// Native chapter editor: no-JS form, live preview, time picker and reflow.
import { chromium } from 'playwright';
import AxeBuilder from '@axe-core/playwright';

const [url, screenshotPath] = process.argv.slice(2);
if (!url || !process.env.PLAMENU_E2E_USER || !process.env.PLAMENU_E2E_PASSWORD) {
  throw new Error('expected chapter editor URL and author credentials');
}
const browser = await chromium.launch();
try {
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.goto(new URL('/login', url).href);
  await page.locator('input[name="identifier"]').fill(process.env.PLAMENU_E2E_USER);
  await page.locator('input[name="password"]').fill(process.env.PLAMENU_E2E_PASSWORD);
  await Promise.all([
    page.waitForURL(next => next.pathname === '/'),
    page.locator('button[type="submit"]').click(),
  ]);
  await page.goto(url);
  const form = page.locator('form[data-chapter-editor]');
  await form.waitFor({ state: 'visible' });
  const textarea = form.locator('textarea[name="chapters"]');
  await textarea.fill('bad chapter line');
  if (!await textarea.evaluate(node => !!node.validationMessage)) throw new Error('invalid chapter line accepted');
  await textarea.fill('0:00 Opening scene\n0:06 Middle scene');
  if (await textarea.evaluate(node => !!node.validationMessage)) throw new Error('valid chapter lines rejected');
  if (await form.locator('[data-chapter-preview] li').count() !== 2) throw new Error('chapter preview missing');
  const current = form.locator('[data-chapter-current]');
  if (!await current.isVisible()) throw new Error('current-time helper inaccessible');
  const audit = await new AxeBuilder({ page }).include('#chapter-editor').analyze();
  const severe = audit.violations.filter(item => ['critical', 'serious'].includes(item.impact));
  if (severe.length) throw new Error(`chapter editor accessibility: ${JSON.stringify(severe)}`);
  if (screenshotPath) {
    await form.scrollIntoViewIfNeeded();
    await page.screenshot({ path: screenshotPath, fullPage: true });
  }
  await page.setViewportSize({ width: 390, height: 844 });
  await page.reload();
  const mobileOverflow = await page.evaluate(() => document.documentElement.scrollWidth > innerWidth + 1);
  const mobileAudit = await new AxeBuilder({ page }).include('#chapter-editor').analyze();
  const mobileSevere = mobileAudit.violations.filter(item => ['critical', 'serious'].includes(item.impact));
  if (mobileOverflow || mobileSevere.length) throw new Error(JSON.stringify({mobileOverflow, mobileSevere}));
  if (screenshotPath?.endsWith('.png')) {
    await page.screenshot({path: screenshotPath.replace(/\.png$/, '-mobile.png'), fullPage: true});
  }
  await page.emulateMedia({colorScheme: 'dark'});
  await page.waitForTimeout(600);
  const darkAudit = await new AxeBuilder({ page }).include('#chapter-editor').analyze();
  const darkSevere = darkAudit.violations.filter(item => ['critical', 'serious'].includes(item.impact));
  if (darkSevere.length || errors.length) throw new Error(JSON.stringify({darkSevere, errors}));
  if (screenshotPath?.endsWith('.png')) {
    await page.screenshot({path: screenshotPath.replace(/\.png$/, '-mobile-dark.png'), fullPage: true});
  }
  console.log(JSON.stringify({preview: 2, currentTimeHelper: true, severe: severe.length,
    mobileSevere: mobileSevere.length, darkSevere: darkSevere.length, mobileOverflow, errors}));
  await context.close();
} finally {
  await browser.close();
}
