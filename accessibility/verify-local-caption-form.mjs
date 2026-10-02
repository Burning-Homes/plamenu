// JS enhancement of the native, no-JS-capable draft caption form.
import { chromium } from 'playwright';
import AxeBuilder from '@axe-core/playwright';
import { openDetailsFor, chooseVideoLanguage } from './session.mjs';

const [reviewUrl, screenshotPath] = process.argv.slice(2);
if (!reviewUrl || !process.env.PLAMENU_E2E_USER || !process.env.PLAMENU_E2E_PASSWORD) {
  throw new Error('expected review URL and local author credentials');
}
const browser = await chromium.launch();
try {
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.goto(new URL('/login', reviewUrl).href);
  await page.locator('input[name="identifier"]').fill(process.env.PLAMENU_E2E_USER);
  await page.locator('input[name="password"]').fill(process.env.PLAMENU_E2E_PASSWORD);
  await Promise.all([
    page.waitForURL(url => url.pathname === '/'),
    page.locator('button[type="submit"]').click(),
  ]);
  await page.goto(reviewUrl);
  const form = page.locator('form[data-caption-upload]');
  await openDetailsFor(form);
  await form.waitFor({ state: 'visible' });
  await chooseVideoLanguage(form, 'fr');
  const file = form.getByLabel('Captions (WebVTT)');
  const status = form.locator('[data-caption-status]');
  await file.setInputFiles({ name: 'wrong.vtt', mimeType: 'text/vtt', buffer: Buffer.from('Not WebVTT') });
  await page.waitForFunction(() => document.querySelector('[data-caption-status]')?.textContent?.includes('Choose a UTF-8 WebVTT'));
  if (!await file.evaluate(input => !!input.validationMessage)) throw new Error('invalid VTT was not rejected before submit');
  await file.setInputFiles({ name: 'good.vtt', mimeType: 'text/vtt',
    buffer: Buffer.from('WEBVTT\n\n00:00:00.000 --> 00:00:08.000\nBonjour de Plamenu\n') });
  await page.waitForFunction(() => document.querySelector('[data-caption-status]')?.textContent?.includes('Timed cues found: 1'));
  if (await file.evaluate(input => !!input.validationMessage)) throw new Error('valid VTT stayed invalid');
  const audit = await new AxeBuilder({ page }).include('#caption-languages').analyze();
  const severe = audit.violations.filter(item => ['critical', 'serious'].includes(item.impact));
  if (severe.length) throw new Error(`caption form accessibility violations: ${JSON.stringify(severe)}`);
  if (screenshotPath) {
    await form.scrollIntoViewIfNeeded();
    await page.screenshot({ path: screenshotPath, fullPage: true });
  }
  const preview = await status.textContent();
  await page.setViewportSize({ width: 390, height: 844 });
  await page.reload();
  await openDetailsFor(form);
  await form.waitFor({ state: 'visible' });
  const mobileOverflow = await page.evaluate(() => document.documentElement.scrollWidth > innerWidth + 1);
  const mobileAudit = await new AxeBuilder({ page }).include('#caption-languages').analyze();
  const mobileSevere = mobileAudit.violations.filter(item => ['critical', 'serious'].includes(item.impact));
  if (mobileOverflow || mobileSevere.length) {
    throw new Error(`mobile caption form failed: ${JSON.stringify({ mobileOverflow, mobileSevere })}`);
  }
  if (screenshotPath?.endsWith('.png')) {
    await page.evaluate(() => document.activeElement?.blur());
    await page.screenshot({ path: screenshotPath.replace(/\.png$/, '-mobile.png'), fullPage: true });
  }
  await page.emulateMedia({ colorScheme: 'dark' });
  // Colour-scheme changes animate the control palette; audit the settled
  // theme, not a single low-contrast in-between transition frame.
  await page.waitForTimeout(600);
  const darkAudit = await new AxeBuilder({ page }).include('#caption-languages').analyze();
  const darkSevere = darkAudit.violations.filter(item => ['critical', 'serious'].includes(item.impact));
  if (darkSevere.length) {
    throw new Error(`dark caption form accessibility violations: ${JSON.stringify(darkSevere)}`);
  }
  if (screenshotPath?.endsWith('.png')) {
    await page.screenshot({ path: screenshotPath.replace(/\.png$/, '-mobile-dark.png'), fullPage: true });
  }
  const result = { preview, severeAccessibilityViolations: severe.length,
    mobileSevereAccessibilityViolations: mobileSevere.length,
    darkSevereAccessibilityViolations: darkSevere.length, mobileOverflow, errors };
  if (errors.length) throw new Error(JSON.stringify(result));
  console.log(JSON.stringify(result));
  await context.close();
} finally {
  await browser.close();
}
