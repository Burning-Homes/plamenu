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
