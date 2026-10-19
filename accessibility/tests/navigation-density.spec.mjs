import { readFileSync } from 'node:fs';
import { expect, test } from '@playwright/test';

const stylesheet = readFileSync(new URL('../../crates/server/src/web/assets/app.css', import.meta.url), 'utf8');
const script = readFileSync(new URL('../../crates/server/src/web/assets/app.js', import.meta.url), 'utf8');
const layout = readFileSync(new URL('../../crates/server/src/web/layout.rs', import.meta.url), 'utf8');
const primary = layout.slice(layout.indexOf('fn primary_nav('), layout.indexOf('fn anon_primary_nav('));
const staffCount = [...primary.matchAll(/\(nav_link\(|a\.nav-link\s/g)].length;
const icon = '<svg class="icon" viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9"/></svg>';
const labels = ['Home', 'Live feeds', 'Search', 'Notifications', 'Messages', 'Bookmarks', 'Favourites', 'Lists', 'Groups', 'Music', 'Videos', 'Apps', 'People', 'Discover', 'Settings', 'Admin'];
const menu = count => Array.from({ length: count }, (_, index) => `<a class="nav-link" href="#destination-${index}" aria-label="${labels[index] || `Destination ${index}`}">${icon}<span class="nav-link__label">${labels[index] || `Destination ${index}`}</span></a>`).join('');
const contents = count => `<a class="brand" href="#home"><span class="brand__mark">P</span><span class="brand__name">Plamenu</span></a>
  <nav class="nav" aria-label="Primary">${menu(count)}</nav><a class="compose-btn" href="#compose">${icon}<span>New post</span></a>
  <div class="account"><a class="account__link" href="#profile"><img class="account__avatar" alt="" width="32" height="32" src="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg'/%3E"><span class="account__name">Also lnkr</span><span class="account__handle">@lnkr</span></a><a class="account__switch" href="#switch" aria-label="Switch account">${icon}</a><form class="logout"><button class="logout__btn" title="Sign out">${icon}</button></form></div>`;

async function fixture(page, count = staffCount, { holdBundle = false } = {}) {
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  let release;
  const gate = new Promise(resolve => { release = resolve; });
  await page.route('https://plamenu-nav.test/**', async route => {
    const path = new URL(route.request().url()).pathname;
    if (path === '/assets/app.css') return route.fulfill({ contentType: 'text/css', body: stylesheet });
    if (path === '/assets/app.js') {
      if (holdBundle) await gate;
      return route.fulfill({ contentType: 'text/javascript', body: script });
    }
    if (path !== '/fixture') return route.fulfill({ status: 404, body: '' });
    const tall = new URL(route.request().url()).searchParams.has('long');
    const feed = tall ? '<p>Feed item</p>'.repeat(1000) : '<p>Feed content</p>';
    return route.fulfill({ contentType: 'text/html', body: `<!doctype html><html lang="en"><head><meta name="viewport" content="width=device-width,initial-scale=1"><title>Navigation fit</title><link rel="stylesheet" href="/assets/app.css"><script defer src="/assets/app.js"></script></head><body>
      <div class="app-shell"><aside class="sidebar">${contents(count)}</aside><main class="app-main" ${tall ? 'style="min-height:20000px"' : ''}>${feed}</main></div>
      <details class="drawer"><summary class="drawer__toggle">Menu</summary><div class="drawer__scrim"><div class="drawer__panel">${contents(count)}</div></div></details>
      </body></html>` });
  });
  await page.goto('https://plamenu-nav.test/fixture', { waitUntil: holdBundle ? 'commit' : 'load' });
  await expect.poll(async () => page.locator('.sidebar').evaluate(panel => getComputedStyle(panel).position)).toBe('sticky');
  return { errors, release };
}

async function state(page, selector = '.sidebar') {
  return page.locator(selector).evaluate(panel => {
    const nav = panel.querySelector('.nav');
    return {
      fits: nav.scrollHeight <= nav.clientHeight + 1,
      row: nav.querySelector('.nav-link').getBoundingClientRect().height,
      navScrollTop: nav.scrollTop,
      horizontalOverflow: nav.scrollWidth - nav.clientWidth,
      chromeVisible: ['.brand', '.compose-btn', '.account'].every(selector => {
        const element = panel.querySelector(selector);
        return !element.getClientRects().length || element.getBoundingClientRect().bottom <= panel.getBoundingClientRect().bottom + 1;
      }),
    };
  });
}

async function expectFits(page, selector = '.sidebar') {
  expect((await state(page, selector)).fits).toBe(true);
  expect((await state(page, selector)).chromeVisible).toBe(true);
}

async function setCount(page, count) {
  await page.locator('.sidebar .nav').evaluate((nav, html) => { nav.innerHTML = html; }, menu(count));
}

async function geometry(page) {
  return page.locator('.sidebar').evaluate(panel => Object.fromEntries(
    ['.brand', '.nav', '.nav-link', '.compose-btn', '.account'].map(selector => {
      const rect = panel.querySelector(selector).getBoundingClientRect();
      return [selector, { x: rect.x, y: rect.y, width: rect.width, height: rect.height }];
    }),
  ));
}

// Run every layout contract in both ordinary and no-script projects. Browser
// evaluation below is test instrumentation; the no-script page executes no JS.
for (const prefix of ['', '@no-js ']) {
  test(`${prefix}navigation adapts to added, removed and hidden destinations`, async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 975 });
    const { errors } = await fixture(page, 16);
    await expectFits(page);
    const initial = (await state(page)).row;
    await page.locator('.sidebar .account__switch').focus();
    await setCount(page, 20);
    await expectFits(page);
    expect((await state(page)).row).toBeLessThan(initial);
    await expect(page.locator('.sidebar .account__switch')).toBeFocused();
    await page.locator('.sidebar .nav-link').evaluateAll(links => { links.slice(4).forEach(link => { link.hidden = true; }); });
    await expectFits(page);
    const roomy = (await state(page)).row;
    expect(roomy).toBeGreaterThan(initial);
    await page.locator('.sidebar .nav-link').evaluateAll(links => { links.forEach(link => { link.hidden = false; }); });
    await expectFits(page);
    expect((await state(page)).row).toBeLessThan(initial);
    await setCount(page, 4);
    expect((await state(page)).row).toBe(roomy);
    expect(errors).toEqual([]);
  });

  test(`${prefix}current menus fit resized windows, enlarged fonts and wrapped labels`, async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 975 });
    const { errors } = await fixture(page);
    for (const count of [staffCount, staffCount - 1]) {
      await setCount(page, count);
      const rows = [];
      for (const height of [1200, 975, 886, 800, 752]) {
        await page.setViewportSize({ width: 1440, height });
        await expectFits(page);
        rows.push((await state(page)).row);
      }
      expect(rows[0]).toBeGreaterThan(rows[1]);
      expect(rows[1]).toBeGreaterThan(rows[3]);
    }
    await page.setViewportSize({ width: 1440, height: 975 });
    await setCount(page, 16);
    await page.evaluate(() => { document.documentElement.style.fontSize = '20px'; });
    await expectFits(page);
    await page.locator('.sidebar .nav-link__label').first().evaluate(label => { label.textContent = 'A long translated navigation label that wraps across several lines'; });
    const labelFits = await page.locator('.sidebar .nav-link').first().evaluate(link => link.scrollHeight <= link.clientHeight + 1);
    expect(labelFits).toBe(true);
    expect((await state(page)).chromeVisible).toBe(true);
    await page.locator('.sidebar .logout__btn').focus();
    await expect(page.locator('.sidebar .logout__btn')).toBeInViewport();
    expect(errors).toEqual([]);
  });

  test(`${prefix}oversized menus scroll without moving compose or account controls`, async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 700 });
    const { errors } = await fixture(page, 40);
    const before = await geometry(page);
    expect((await state(page)).fits).toBe(false);
    expect((await state(page)).chromeVisible).toBe(true);
    await page.locator('.sidebar .nav-link').last().focus();
    await expect(page.locator('.sidebar .nav-link').last()).toBeInViewport();
    expect((await state(page)).navScrollTop).toBeGreaterThan(0);
    const after = await geometry(page);
    expect(after['.compose-btn']).toEqual(before['.compose-btn']);
    expect(after['.account']).toEqual(before['.account']);
    await setCount(page, 4);
    await expectFits(page);
    expect((await state(page)).navScrollTop).toBe(0);
    expect(errors).toEqual([]);
  });

  test(`${prefix}drawer maintains touch targets and footer access in short viewports`, async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 975 });
    const { errors } = await fixture(page, 16);
    await page.locator('.drawer__toggle').click();
    await expectFits(page, '.drawer__panel');
    await page.setViewportSize({ width: 390, height: 500 });
    const measured = await state(page, '.drawer__panel');
    expect(measured.fits).toBe(false);
    expect(measured.row).toBeGreaterThanOrEqual(40);
    expect(measured.chromeVisible).toBe(true);
    await page.locator('.drawer__panel .nav-link').last().focus();
    await expect(page.locator('.drawer__panel .nav-link').last()).toBeInViewport();
    await expect(page.locator('.drawer__panel .logout__btn')).toBeInViewport();
    expect(errors).toEqual([]);
  });

  test(`${prefix}sidebar is stable before scripts load and when the feed becomes long`, async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 975 });
    const { errors, release } = await fixture(page, 16, { holdBundle: true });
    try {
      await expectFits(page);
      const before = await geometry(page);
      // Let actual first-paint frames happen while the enhancement bundle is held.
      let earlyFrames;
      if (prefix) {
        // No-script clients cannot run page rAF callbacks. Sample compositor
        // frames through the screenshot protocol after their document is loaded.
        earlyFrames = [];
        for (let sample = 0; sample < 4; sample++) {
          await page.screenshot();
          earlyFrames.push((await geometry(page))['.nav-link'].height);
        }
      } else {
        // Font-readiness promises may wait for deferred scripts in some engines,
        // so sample rAF directly while the bundle is deliberately held back.
        earlyFrames = await page.locator('.sidebar .nav-link').first().evaluate(link => new Promise(resolve => {
          const heights = [];
          const sample = () => { heights.push(link.getBoundingClientRect().height); if (heights.length < 4) requestAnimationFrame(sample); else resolve(heights); };
          requestAnimationFrame(sample);
        }));
      }
      expect(new Set(earlyFrames).size).toBe(1);
      expect(earlyFrames[0]).toBe(before['.nav-link'].height);
      await page.locator('.app-main').evaluate(main => { main.style.height = '20000px'; });
      expect(await geometry(page)).toEqual(before);
      release();
      await page.waitForLoadState('load');
      expect(await geometry(page)).toEqual(before);
      // Repeat a full navigation with a tall feed in the initial document.
      await page.goto('https://plamenu-nav.test/fixture?long=1');
      const reloaded = await geometry(page);
      expect(reloaded).toEqual(before);
      await page.locator('.app-main').evaluate(main => { main.style.height = '30000px'; });
      expect(await geometry(page)).toEqual(before);
      expect(errors).toEqual([]);
    } finally { release(); }
  });

  test(`${prefix}icon rail has no horizontal overflow while scrolling`, async ({ page }) => {
    await page.setViewportSize({ width: 800, height: 500 });
    const { errors } = await fixture(page, 40);
    expect((await state(page)).horizontalOverflow).toBeLessThanOrEqual(1);
    await page.locator('.sidebar .nav-link').last().focus();
    await expect(page.locator('.sidebar .nav-link').last()).toBeInViewport();
    expect(errors).toEqual([]);
  });
}
