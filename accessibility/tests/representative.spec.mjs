import { expect, test } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const publicPages = [
  ['landing', '/'],
  ['public timeline', '/public'],
  ['explore', '/explore'],
  ['people', '/people'],
  ['search', '/search'],
  ['sign in', '/login'],
  ['registration', '/signup'],
  ['rules', '/rules'],
  ['staff', '/staff'],
];

const authenticatedPages = [
  ['home timeline', '/'],
  ['composer', '/compose'],
  ['notifications', '/notifications'],
  ['settings', '/settings'],
  ['profile settings', '/settings/profile'],
  ['account settings', '/settings/account'],
  ['security settings', '/settings/security'],
  ['Webxdc launcher', '/webxdc'],
];

const noJavaScriptPages = [
  ['landing', '/'],
  ['sign in', '/login'],
  ['registration', '/signup'],
  ['rules', '/rules'],
];

async function assertPageStructure(page, label) {
  await expect(page.locator('html')).toHaveAttribute('lang', /\S+/);
  await expect(page).toHaveTitle(/\S+/);
  await expect(page.locator('main#main-content')).toHaveCount(1);
  await expect(page.locator('a.skip-link[href="#main-content"]')).toHaveCount(1);
  const duplicateIds = await page.locator('[id]').evaluateAll((nodes) => {
    const seen = new Set();
    return nodes.map((node) => node.id).filter((id) => seen.has(id) || !seen.add(id));
  });
  expect(duplicateIds, `${label} has duplicate IDs`).toEqual([]);
}

async function scan(page, label, disabledRules = []) {
  await assertPageStructure(page, label);
  let builder = new AxeBuilder({ page })
    .withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa']);
  if (disabledRules.length) {
    builder = builder.disableRules(disabledRules);
  }
  const results = await builder.analyze();
  const violations = results.violations.map((violation) => ({
    id: violation.id,
    impact: violation.impact,
    targets: violation.nodes.slice(0, 10).map((node) => node.target.join(' ')),
    omitted_targets: Math.max(0, violation.nodes.length - 10),
  }));
  expect(violations, `${label} axe violations`).toEqual([]);
}

async function visit(page, label, url) {
  const response = await page.goto(url, { waitUntil: 'networkidle' });
  expect(response?.status(), `${label} response`).toBeLessThan(400);
  await scan(page, label);
}

async function signIn(page) {
  await page.goto('/login');
  await page.locator('input[name="identifier"]').fill(
    process.env.A11Y_USERNAME || 'developer',
  );
  await page.locator('input[name="password"]').fill(
    process.env.A11Y_PASSWORD || 'plamenu-development-only',
  );
  await Promise.all([
    page.waitForURL((url) => url.pathname === '/'),
    page.locator('button[type="submit"]').click(),
  ]);
  await expect(page.locator('a.compose-btn[href="/compose"]').first()).toBeAttached();
}

async function publishFixturePost(page, projectName) {
  await page.goto('/compose');
  await page.locator('textarea[name="status"]').fill(
    `Accessibility fixture for ${projectName}`,
  );
  await Promise.all([
    page.waitForURL((url) => url.pathname.startsWith('/@developer/')),
    page.locator('button[name="op"][value="post"]').click(),
  ]);

  const favourite = page.locator('form[data-action="favourite"] button').first();
  await favourite.click();
  await expect(favourite).toHaveClass(/\bis-active\b/);
  const activeStyle = await favourite.evaluate((button) => {
    const control = getComputedStyle(button);
    const marker = getComputedStyle(button, '::after');
    return {
      boxShadow: control.boxShadow,
      markerContent: marker.content,
      markerWidth: Number.parseFloat(marker.width),
      markerHeight: Number.parseFloat(marker.height),
    };
  });
  expect(activeStyle.boxShadow).toBe('none');
  expect(activeStyle.markerContent).not.toBe('none');
  expect(activeStyle.markerWidth).toBeLessThanOrEqual(16);
  expect(activeStyle.markerHeight).toBe(2);
}

test('representative signed-out pages pass WCAG A/AA automated rules', async ({ page }) => {
  for (const [label, url] of publicPages) {
    await visit(page, label, url);
  }
});

test('representative signed-in pages pass WCAG A/AA automated rules', async ({ page }, testInfo) => {
  await signIn(page);
  await publishFixturePost(page, testInfo.project.name);
  for (const [label, url] of authenticatedPages) {
    await visit(page, label, url);
  }
});

test('Russian, dark theme, reduced motion, and forced colours remain operable', async ({ browser }) => {
  const context = await browser.newContext({
    baseURL: process.env.A11Y_BASE_URL || 'https://plamenu.local',
    ignoreHTTPSErrors: true,
    locale: 'ru-RU',
    colorScheme: 'dark',
    reducedMotion: 'reduce',
    serviceWorkers: 'block',
    ...(process.env.A11Y_CANONICAL_ORIGIN
      ? { extraHTTPHeaders: { Origin: process.env.A11Y_CANONICAL_ORIGIN } }
      : {}),
  });
  const page = await context.newPage();
  try {
    await visit(page, 'Russian dark reduced-motion sign in', '/login');
    await expect(page.locator('html')).toHaveAttribute('lang', /^ru(?:-|$)/);

    await page.emulateMedia({ colorScheme: 'dark', reducedMotion: 'reduce', forcedColors: 'active' });
    const response = await page.goto('/login', { waitUntil: 'networkidle' });
    expect(response?.status(), 'Russian forced-colours sign in response').toBeLessThan(400);
    // System colours do not resolve to meaningful RGB pairs for axe. The
    // release protocol measures forced-colours contrast manually; every other
    // applicable automated rule still runs here.
    await scan(page, 'Russian forced-colours sign in', ['color-contrast']);
  } finally {
    await context.close();
  }
});

test('keyboard bypass and 320 CSS-pixel reflow contracts hold', async ({ page }, testInfo) => {
  await page.goto('/rules');
  await page.keyboard.press('Tab');
  await expect(page.locator('.skip-link')).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(page.locator('main#main-content')).toBeFocused();

  if (testInfo.project.name.includes('mobile')) {
    const widths = await page.evaluate(() => ({
      client: document.documentElement.clientWidth,
      scroll: document.documentElement.scrollWidth,
    }));
    expect(widths.scroll, 'page-level horizontal overflow at 320 CSS pixels')
      .toBeLessThanOrEqual(widths.client);

    await signIn(page);
    await page.goto('/compose');

    const selectors = page.locator('[data-compose-menu], [data-compose-combo]');
    for (const wrapper of await selectors.all()) {
      const trigger = wrapper.locator('[data-menu-trigger], [data-combo-trigger]');
      const popup = wrapper.locator('.compose__menu-pop, .compose__combo-pop');
      await trigger.click();
      const box = await popup.boundingBox();
      expect(box, 'open composer selector has a box').not.toBeNull();
      expect(box.x, 'composer selector crosses the viewport start edge')
        .toBeGreaterThanOrEqual(7.9);
      expect(box.x + box.width, 'composer selector crosses the viewport end edge')
        .toBeLessThanOrEqual(widths.client - 7.9);
      await page.keyboard.press('Escape');
    }

    const picker = page.locator(
      '.compose__media-body > input[type="file"].visually-hidden',
    );
    await picker.setInputFiles({
      name: 'sample.png',
      mimeType: 'image/png',
      buffer: Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
    });
    const card = page.locator('.compose__attachment');
    await expect(card).toBeVisible();
    expect(await card.locator('.compose__alt').getAttribute('placeholder')).toBeNull();
    const [cardBox, bodyBox, altBox, decorativeBox] = await Promise.all([
      card.boundingBox(),
      card.locator('.compose__attachment-body').boundingBox(),
      card.locator('.compose__alt').boundingBox(),
      card.locator('.compose__inline.compose__media-extra').boundingBox(),
    ]);
    expect(bodyBox.width).toBeGreaterThan(cardBox.width - 20);
    expect(altBox.width).toBeGreaterThan(cardBox.width - 20);
    expect(decorativeBox.width).toBeGreaterThan(cardBox.width - 20);
  }
});

test('@no-js public pages retain their core document and form contracts', async ({ page }) => {
  for (const [label, url] of noJavaScriptPages) {
    const response = await page.goto(url, { waitUntil: 'domcontentloaded' });
    expect(response?.status(), `${label} without JavaScript response`).toBeLessThan(400);
    // axe itself requires JavaScript. This project instead verifies the
    // server-rendered structural and native-control fallback contracts.
    await assertPageStructure(page, `${label} without JavaScript`);
  }
  await page.goto('/login');
  await expect(page.locator('html')).toHaveClass(/\bno-js\b/);
  await expect(page.locator('form.auth-form')).toHaveAttribute('action', '/login');
});
