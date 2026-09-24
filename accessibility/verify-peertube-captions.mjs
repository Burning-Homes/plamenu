// Remote PeerTube WebVTT tracks in Plamenu's real first-party player.
import { chromium } from 'playwright';
import AxeBuilder from '@axe-core/playwright';

const [watchUrl, expectedCue = 'Bonjour de PeerTube', expectedLanguagesJson = '["en","fr"]', seekMsRaw = '0', seekCue] = process.argv.slice(2);
const expectedLanguages = JSON.parse(expectedLanguagesJson);
const seekMs = Number(seekMsRaw);
if (!watchUrl) throw new Error('expected Plamenu watch URL');
const browser = await chromium.launch();
try {
  const noJs = await browser.newContext({ ignoreHTTPSErrors: true, javaScriptEnabled: false });
  const staticPage = await noJs.newPage();
  await staticPage.goto(watchUrl);
  const staticTracks = await staticPage.locator('video track[kind="captions"]').evaluateAll(nodes =>
    nodes.map(node => ({ lang: node.srclang, src: node.getAttribute('src') })));
  if (JSON.stringify(staticTracks.map(track => track.lang)) !== JSON.stringify(expectedLanguages)) {
    throw new Error(`no-JS captions missing: ${JSON.stringify(staticTracks)}`);
  }
  if (staticTracks.some(track => !track.src.startsWith('https://plamenu.local/media/'))) {
    throw new Error(`remote URL leaked into track markup: ${JSON.stringify(staticTracks)}`);
  }
  const transcriptLinks = staticPage.locator('.media__caption-transcripts a');
  if (await transcriptLinks.count() !== expectedLanguages.length) {
    throw new Error('no-JS caption transcript links missing');
  }
  const frenchTranscriptUrl = await transcriptLinks.filter({ hasText: 'fr' }).getAttribute('href');
  await staticPage.goto(frenchTranscriptUrl);
  if (!await staticPage.getByText(expectedCue, { exact: true }).count()) {
    throw new Error('no-JS caption transcript text missing');
  }
  const staticSeek = await staticPage.locator(`a[data-transcript-seek-ms="${seekMs}"]`).getAttribute('href');
  if (!staticSeek.endsWith(`#t=${Math.floor(seekMs / 1000)}.${String(seekMs % 1000).padStart(3, '0')}`)
      || !staticSeek.startsWith('https://plamenu.local/')) {
    throw new Error(`no-JS media-fragment fallback invalid: ${staticSeek}`);
  }
  await noJs.close();

  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.goto(watchUrl);
  const player = page.locator('video').first();
  await player.waitFor({ state: 'attached' });
  await player.evaluate(video => {
    video.muted = true;
    for (const track of video.textTracks) track.mode = track.language === 'fr' ? 'showing' : 'disabled';
    video.play().catch(() => {});
  });
  try {
    await page.waitForFunction(cue => [...document.querySelector('video')?.textTracks || []]
      .some(track => track.language === 'fr' && track.activeCues?.[0]?.text === cue),
    expectedCue, { timeout: 45000 });
  } catch (cause) {
    const state = await player.evaluate(video => ({
      time: video.currentTime, ready: video.readyState, error: video.error?.message,
      tracks: [...video.textTracks].map(track => ({ language: track.language, mode: track.mode,
        cues: track.cues?.length, active: track.activeCues?.[0]?.text })),
    }));
    throw new Error(`French caption cue not active: ${JSON.stringify({ state, errors })}`, { cause });
  }
  await page.goto(frenchTranscriptUrl);
  const transcript = page.locator('.caption-transcript');
  if (!await transcript.getByText(seekCue || expectedCue).count()) {
    throw new Error('French closing cue not rendered in transcript');
  }
  await page.locator(`a[data-transcript-seek-ms="${seekMs}"]`).focus();
  await page.keyboard.press('Enter');
  await page.waitForFunction(expectedSeconds => {
    const video = document.querySelector('.caption-transcript video');
    return video && video.readyState >= HTMLMediaElement.HAVE_METADATA
      && video.currentTime >= expectedSeconds - 0.1 && video.currentTime < expectedSeconds + 1;
  }, seekMs / 1000, { timeout: 45000 });
  if (new URL(page.url()).pathname !== new URL(frenchTranscriptUrl).pathname) {
    throw new Error('enhanced transcript seek navigated away from the readable page');
  }
  const desktopAudit = await new AxeBuilder({ page }).include('.caption-transcript').analyze();
  const severe = audit => audit.violations.filter(item => ['critical', 'serious'].includes(item.impact));
  if (severe(desktopAudit).length) throw new Error(`desktop transcript axe violations: ${JSON.stringify(severe(desktopAudit))}`);
  await page.screenshot({ path: '/tmp/ap09-caption-transcript-desktop.png', fullPage: true });
  await page.setViewportSize({ width: 390, height: 844 });
  const mobileOverflow = await page.evaluate(() => document.documentElement.scrollWidth > innerWidth + 1);
  const mobileAudit = await new AxeBuilder({ page }).include('.caption-transcript').analyze();
  if (mobileOverflow || severe(mobileAudit).length) throw new Error(`mobile transcript accessibility: ${JSON.stringify({mobileOverflow, violations: severe(mobileAudit)})}`);
  await page.screenshot({ path: '/tmp/ap09-caption-transcript-mobile.png', fullPage: true });
  await page.emulateMedia({ colorScheme: 'dark' });
  await page.waitForTimeout(600);
  const darkAudit = await new AxeBuilder({ page }).include('.caption-transcript').analyze();
  if (severe(darkAudit).length) throw new Error(`dark transcript axe violations: ${JSON.stringify(severe(darkAudit))}`);
  console.log(JSON.stringify({ noJsTracks: staticTracks, frenchCue: true, pageErrors: errors }));
  await context.close();
} finally {
  await browser.close();
}
