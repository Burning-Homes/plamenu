import { readFileSync } from 'node:fs';
import { expect, test } from '../fixtures.mjs';

const stylesheet = readFileSync(new URL('../../crates/server/src/web/assets/app.css', import.meta.url), 'utf8');
const script = readFileSync(new URL('../../crates/server/src/web/assets/app.js', import.meta.url), 'utf8');
const origin = 'https://plamenu-lightbox.test';
const image = '<svg xmlns="http://www.w3.org/2000/svg" width="400" height="200"><rect width="400" height="200" fill="#789"/></svg>';
const gallery = `<div class="status__media"><a class="media__link" href="/image.svg" target="_blank" rel="noopener noreferrer"><img src="/image.svg" width="400" height="200" alt="Original post image"></a></div>`;
const thread = `<main class="app-main" tabindex="-1" style="max-width:600px;margin:auto">
  <div style="height:400px"></div><article class="status" id="post-op">${gallery}</article>
  <div style="height:1800px"></div><article class="status" id="post-reply">The directly opened reply</article>
  <div style="height:2000px"></div></main>`;

async function fixture(page, { music = false } = {}) {
  const errors = [];
  const requests = [];
  page.on('pageerror', error => errors.push(error.message));
  // Context routing also handles the first request of the native image popup.
  await page.context().route(`${origin}/**`, route => {
    const path = new URL(route.request().url()).pathname;
    if (path === '/assets/app.css') return route.fulfill({ contentType: 'text/css', body: stylesheet });
    if (path === '/assets/app.js') return route.fulfill({ contentType: 'text/javascript', body: script });
    if (path === '/image.svg') return route.fulfill({ contentType: 'image/svg+xml', body: image });
    requests.push(path);
    const player = `<main class="app-main" tabindex="-1"><section class="music-player-page"><h1>Music player</h1><div class="music-player-media"><audio class="music-player-audio" data-player-bound="1"></audio></div><a href="/@fixture/1">Open thread</a></section></main>`;
    return route.fulfill({ contentType: 'text/html', body: `<!doctype html><html lang="en"><head><meta name="viewport" content="width=device-width,initial-scale=1"><title>Lightbox scrolling</title><link rel="stylesheet" href="/assets/app.css"><script defer src="/assets/app.js"></script></head><body>${path === '/music/player' ? player : path === '/before' ? '<main>Previous page</main>' : thread}</body></html>` });
  });
  await page.goto(`${origin}/before`);
  if (music) {
    await page.goto(`${origin}/music/player`);
    await page.getByRole('link', { name: 'Open thread' }).click();
    await expect(page.locator('#post-op')).toBeVisible();
    await expect(page.locator('.music-dock')).toBeAttached();
  } else {
    await page.goto(`${origin}/@fixture/1#post-reply`);
    await expect.poll(() => page.evaluate(() => scrollY)).toBeGreaterThan(1800);
  }
  return { errors, requests };
}

async function openAtOriginalPost(page) {
  const tile = page.locator('a.media__link');
  await tile.evaluate(el => el.scrollIntoView({ block: 'center' }));
  // Capture every painted frame, including while opening, so a transient jump
  // cannot pass merely because a later callback scrolls back to the right place.
  const position = await page.evaluate(() => {
    window.lightboxScrollSamples = [];
    window.sampleLightboxScroll = true;
    function sample() {
      lightboxScrollSamples.push({ x: scrollX, y: scrollY });
      if (sampleLightboxScroll) requestAnimationFrame(sample);
    }
    sample();
    return { x: scrollX, y: scrollY };
  });
  await tile.click();
  await expect(page.getByRole('dialog', { name: 'Media viewer' })).toBeVisible();
  return position;
}

async function closeViewer(page, method) {
  const dialog = page.getByRole('dialog', { name: 'Media viewer' });
  if (method === 'button') await dialog.getByRole('button', { name: 'Close', exact: true }).click();
  if (method === 'escape') await page.keyboard.press('Escape');
  if (method === 'backdrop') await dialog.locator('.lightbox__stage').click({ position: { x: 5, y: 5 } });
  if (method === 'back') await page.evaluate(() => history.back());
  await expect(dialog).toBeHidden();
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
}

async function expectPosition(page, position) {
  const samples = await page.evaluate(() => {
    window.sampleLightboxScroll = false;
    return [...lightboxScrollSamples, { x: scrollX, y: scrollY }];
  });
  expect(samples.length).toBeGreaterThan(2);
  expect(samples.every(point => Math.abs(point.x - position.x) <= 1 && Math.abs(point.y - position.y) <= 1), JSON.stringify(samples)).toBe(true);
  await expect(page.locator('a.media__link')).toBeFocused();
}

for (const method of ['button', 'escape', 'backdrop', 'back']) {
  test(`first lightbox ${method} close preserves scroll after opening a reply directly`, async ({ page }) => {
    const { errors } = await fixture(page);
    const originalState = { existing: 'page state', scrollY: 2345 };
    await page.evaluate(state => history.replaceState(state, '', location.href), originalState);
    const position = await openAtOriginalPost(page);
    await closeViewer(page, method);
    await expectPosition(page, position);
    expect(await page.evaluate(() => history.state)).toEqual(originalState);
    expect(await page.evaluate(() => history.scrollRestoration)).toBe('auto');
    await expect(page).toHaveURL(`${origin}/@fixture/1#post-reply`);
    expect(errors).toEqual([]);
  });
}

test('lightbox closes repeatedly without taking ownership of manual scroll restoration', async ({ page }) => {
  const { errors } = await fixture(page);
  await page.evaluate(() => { history.scrollRestoration = 'manual'; });
  for (const method of ['button', 'back', 'escape']) {
    const position = await openAtOriginalPost(page);
    await closeViewer(page, method);
    await expectPosition(page, position);
    expect(await page.evaluate(() => history.scrollRestoration)).toBe('manual');
  }
  expect(errors).toEqual([]);
});

test('lightbox history does not reload the thread while a music player is docked', async ({ page }) => {
  const { errors, requests } = await fixture(page, { music: true });
  const before = requests.filter(path => path === '/@fixture/1').length;
  const position = await openAtOriginalPost(page);
  await closeViewer(page, 'back');
  await expectPosition(page, position);
  expect(requests.filter(path => path === '/@fixture/1')).toHaveLength(before);
  expect(await page.evaluate(() => history.scrollRestoration)).toBe('manual');
  expect(errors).toEqual([]);
});

test('duplicate close input consumes only the viewer entry and ordinary navigation still restores scroll', async ({ page }) => {
  const { errors } = await fixture(page);
  const position = await openAtOriginalPost(page);
  await page.locator('.lightbox__close').evaluate(button => { button.click(); button.click(); });
  await expect(page.getByRole('dialog', { name: 'Media viewer' })).toBeHidden();
  await expect.poll(() => page.evaluate(() => history.scrollRestoration)).toBe('auto');
  await expectPosition(page, position);
  await page.goBack();
  await expect(page).toHaveURL(`${origin}/before`);
  await page.goForward();
  await expect(page).toHaveURL(`${origin}/@fixture/1#post-reply`);
  await expect.poll(async () => Math.abs((await page.evaluate(() => scrollY)) - position.y)).toBeLessThanOrEqual(1);
  expect(errors).toEqual([]);
});

test('an immediate reopen keeps restoration suspended until the new viewer closes', async ({ page }) => {
  const { errors } = await fixture(page);
  const position = await openAtOriginalPost(page);
  await page.locator('.lightbox__close').evaluate(button => {
    const tile = document.querySelector('a.media__link');
    tile.addEventListener('focus', () => setTimeout(() => tile.click(), 0), { once: true });
    window.lightboxTraversals = 0;
    window.addEventListener('popstate', () => { lightboxTraversals++; });
    button.click();
  });
  await expect.poll(() => page.evaluate(() => window.lightboxTraversals)).toBe(1);
  await expect(page.getByRole('dialog', { name: 'Media viewer' })).toBeVisible();
  expect(await page.evaluate(() => history.scrollRestoration)).toBe('manual');
  await closeViewer(page, 'escape');
  await expectPosition(page, position);
  expect(await page.evaluate(() => history.scrollRestoration)).toBe('auto');
  expect(errors).toEqual([]);
});

test('returning focus to an offscreen image does not scroll the document', async ({ page }) => {
  const { errors } = await fixture(page);
  await openAtOriginalPost(page);
  const position = await page.evaluate(() => {
    window.scrollTo(0, 1500);
    window.lightboxScrollSamples = [];
    return { x: scrollX, y: scrollY };
  });
  await closeViewer(page, 'button');
  await expectPosition(page, position);
  expect(errors).toEqual([]);
});

test('@no-js image links open their file without changing the reply page scroll', async ({ page }) => {
  const { errors } = await fixture(page);
  const tile = page.locator('a.media__link');
  await tile.evaluate(el => el.scrollIntoView({ block: 'center' }));
  const before = await page.evaluate(() => scrollY);
  const popup = page.waitForEvent('popup');
  await tile.click();
  const imagePage = await popup;
  await imagePage.waitForLoadState();
  expect(imagePage.url()).toBe(`${origin}/image.svg`);
  await imagePage.close();
  expect(await page.evaluate(() => scrollY)).toBe(before);
  await expect(page.locator('.lightbox')).toHaveCount(0);
  await expect(page).toHaveURL(`${origin}/@fixture/1#post-reply`);
  expect(errors).toEqual([]);
});
