import { defineConfig, devices } from '@playwright/test';

const baseURL = process.env.A11Y_BASE_URL || 'https://plamenu.local';
const canonicalOrigin = process.env.A11Y_CANONICAL_ORIGIN;

const desktop = {
  viewport: { width: 1440, height: 900 },
};

const mobile = {
  viewport: { width: 320, height: 800 },
  isMobile: true,
  hasTouch: true,
};

export default defineConfig({
  testDir: './tests',
  fullyParallel: false,
  forbidOnly: true,
  retries: process.env.CI ? 1 : 0,
  workers: 1,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  outputDir: '../target/accessibility/test-results',
  reporter: [
    ['line'],
    ['./evidence-reporter.mjs'],
  ],
  use: {
    baseURL,
    ignoreHTTPSErrors: true,
    actionTimeout: 10_000,
    navigationTimeout: 20_000,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    serviceWorkers: 'block',
    ...(canonicalOrigin ? { extraHTTPHeaders: { Origin: canonicalOrigin } } : {}),
  },
  projects: [
    { name: 'chromium-desktop', grepInvert: /@no-js/, use: { ...devices['Desktop Chrome'], ...desktop } },
    { name: 'chromium-mobile', grepInvert: /@no-js/, use: { ...devices['Desktop Chrome'], ...mobile } },
    { name: 'firefox-desktop', grepInvert: /@no-js/, use: { ...devices['Desktop Firefox'], ...desktop } },
    { name: 'firefox-mobile', grepInvert: /@no-js/, use: { ...devices['Desktop Firefox'], ...mobile } },
    { name: 'webkit-desktop', grepInvert: /@no-js/, use: { ...devices['Desktop Safari'], ...desktop } },
    { name: 'webkit-mobile', grepInvert: /@no-js/, use: { ...devices['Desktop Safari'], ...mobile } },
    {
      name: 'chromium-no-javascript',
      use: { ...devices['Desktop Chrome'], ...desktop, javaScriptEnabled: false },
      grep: /@no-js/,
    },
  ],
});
