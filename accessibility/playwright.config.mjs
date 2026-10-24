import { defineConfig, devices } from '@playwright/test';
import path from 'node:path';
import {runDirectory} from './gate-evidence.mjs';

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
// Only the isolated fixture proxy consumes this bounded client identity.
// Journeys can select fixed participant, security, recovery or throttle-test roles.
const clientHeaders = project => canonicalOrigin
  ? { Origin: canonicalOrigin, 'X-Plamenu-Fixture-Client': project }
  : {};

export default defineConfig({
  testDir: './tests',
  globalSetup: './audio-setup.mjs',
  fullyParallel: false,
  forbidOnly: true,
  retries: process.env.CI ? 1 : 0,
  workers: 1,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  outputDir: process.env.A11Y_OUTPUT_DIR || path.join(runDirectory, 'test-results'),
  reporter: [
    ['line'],
    ['./evidence-reporter.mjs'],
  ],
  use: {
    baseURL,
    ignoreHTTPSErrors: false,
    actionTimeout: 10_000,
    navigationTimeout: 20_000,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    serviceWorkers: 'block',
    ...(canonicalOrigin ? { extraHTTPHeaders: { Origin: canonicalOrigin } } : {}),
  },
  projects: [
    { name: 'chromium-desktop', grepInvert: /@no-js/, use: { ...devices['Desktop Chrome'], ...desktop, extraHTTPHeaders: clientHeaders('chromium-desktop') } },
    { name: 'chromium-mobile', grepInvert: /@no-js/, use: { ...devices['Desktop Chrome'], ...mobile, extraHTTPHeaders: clientHeaders('chromium-mobile') } },
    { name: 'firefox-desktop', grepInvert: /@no-js/, use: { ...devices['Desktop Firefox'], ...desktop, extraHTTPHeaders: clientHeaders('firefox-desktop') } },
    // Firefox supports a narrow viewport and touch input, but Playwright's
    // isMobile emulation is unsupported there and suppresses mouse pointers.
    { name: 'firefox-mobile', grepInvert: /@no-js/, use: { ...devices['Desktop Firefox'], ...mobile, isMobile: false, extraHTTPHeaders: clientHeaders('firefox-mobile') } },
    { name: 'webkit-desktop', grepInvert: /@no-js/, use: { ...devices['Desktop Safari'], ...desktop, extraHTTPHeaders: clientHeaders('webkit-desktop') } },
    { name: 'webkit-mobile', grepInvert: /@no-js/, use: { ...devices['Desktop Safari'], ...mobile, extraHTTPHeaders: clientHeaders('webkit-mobile') } },
    {
      name: 'chromium-no-javascript',
      use: { ...devices['Desktop Chrome'], ...desktop, javaScriptEnabled: false, extraHTTPHeaders: clientHeaders('chromium-no-javascript') },
      grep: /@no-js/,
    },
  ],
});
