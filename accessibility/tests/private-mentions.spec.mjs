import { test, expect } from '@playwright/test';
import { signIn } from '../session.mjs';

const recipientSessions = new Map();

for (const prefix of ['', '@no-js ']) {
  for (const surface of ['new message', 'inline public-thread reply', 'dedicated public-thread reply']) {
    test(`${prefix}Private mention uses inline mentions in ${surface}`, async ({ page, browser }, info) => {
      await signIn(page);
      const otherContext = await browser.newContext({
        baseURL: process.env.A11Y_BASE_URL,
        extraHTTPHeaders: info.project.use.extraHTTPHeaders,
        javaScriptEnabled: !prefix,
        serviceWorkers: 'block',
      });
      const other = await otherContext.newPage();
      const recipient = 'channel_editor';
      try {
        const cookies = recipientSessions.get(info.project.name);
        if (cookies) {
          await otherContext.addCookies(cookies);
          await other.goto('/');
        } else {
          await other.goto('/login');
          await other.locator('[name=identifier]').fill(recipient);
          await other.locator('[name=password]').fill(process.env.A11Y_PASSWORD || 'plamenu-development-only');
          await other.getByRole('button', { name: 'Sign in', exact: true }).click();
          await expect(other).toHaveURL(url => url.pathname === '/');
          recipientSessions.set(info.project.name, await otherContext.cookies());
        }
        await expect(other).toHaveURL(url => url.pathname === '/');
        if (surface === 'new message') {
          await page.goto('/compose?visibility=direct');
        } else {
          await other.goto('/compose');
          await other.locator('textarea[name=status]').fill('Public conversation ' + Date.now());
          await other.locator('.compose__publish').click();
          await expect(other).toHaveURL(/\/@[^/]+\/\d+/);
          const root = new URL(other.url()).pathname;
          const id = root.split('/').at(-1);
          await page.goto(surface.startsWith('inline') ? root : `/compose?reply=${id}`);
          await expect(page.locator('textarea[name=status]')).toHaveValue(`@${recipient} `);
        }
        const form = page.locator('form[data-compose]');
        const body = form.locator('textarea[name=status]');
        const before = await body.inputValue();
        if (prefix) {
          await form.locator('select[name=visibility]').selectOption('direct');
        } else {
          const visibility = form.locator('[data-compose-menu]:has(select[name=visibility])');
          await visibility.locator('[data-menu-trigger]').click();
          await visibility.getByRole('option', { name: /Private mention/ }).click();
        }
        await expect(body).toHaveValue(before);
        await expect(form.locator('[name=recipients], [data-compose-recipients]')).toHaveCount(0);
        if (!prefix && surface === 'new message') {
          await body.fill('@' + recipient.slice(0, -1));
          await page.getByRole('option').filter({ hasText: '@' + recipient }).click();
          await expect(body).toHaveValue(`@${recipient} `);
        }
        const text = `@${recipient} Private reply ` + Date.now();
        await body.fill(text);
        await form.getByRole('button', { name: 'Preview', exact: true }).click();
        await expect(page.locator('[data-compose-preview] .status')).toContainText('Private reply');
        await expect(body).toHaveValue(text);
        await expect(form.locator('select[name=visibility]')).toHaveValue('direct');
        await expect(form.locator('[name=recipients]')).toHaveCount(0);
        expect(await form.evaluate(element => element.checkValidity())).toBe(true);
        await expect(form.locator('.compose__publish')).toHaveText(surface === 'new message' ? 'Publish' : 'Reply');
        await page.screenshot({ path: info.outputPath('private-mention.png'), fullPage: true, animations: 'disabled' });
        await form.locator('.compose__publish').click();
        await expect(page).toHaveURL(/\/@[^/]+\/\d+/);
        await other.goto(page.url());
        await expect(other.locator('.thread__focus')).toContainText('Private reply');
        // Existing private-thread replies retain their audience without typing
        // the addresses again, and cannot be switched back to public.
        await expect(other.locator('input[name=visibility][value=direct]')).toHaveCount(1);
        await expect(other.locator('select[name=visibility]')).toHaveCount(0);
        await expect(other.locator('[name=recipients]')).toHaveCount(0);
      } finally {
        await otherContext.close();
      }
    });
  }
}
