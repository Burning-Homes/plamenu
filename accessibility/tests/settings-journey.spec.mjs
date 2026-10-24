import { test, expect } from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import { signIn } from '../session.mjs';

async function audit(page, info, name) {
  await page.screenshot({ path: info.outputPath(`${name}.png`), fullPage: true });
  // Axe needs JavaScript; native focus and form checks still run below.
  if (info.project.use.javaScriptEnabled === false) return;
  const result = await new AxeBuilder({ page }).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
  expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
}

async function emailErrors(page, info) {
  await signIn(page);
  await page.goto('/settings/account');
  const form = page.locator('form[action="/web/settings/account/email"]');
  const original = await form.locator('[name=email]').inputValue();
  const email = `settings_${info.project.name}@example.test`;
  await form.locator('[name=email]').fill(email);
  await form.locator('[name=current_password]').fill('incorrect-password');
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.locator('#form-error')).toContainText('current password was incorrect');
  await audit(page, info, 'settings-email-error');
  await expect.soft(form.locator('[name=email]')).toHaveValue(email, { timeout: 1500 });
  await expect.soft(page.locator('#form-error')).toBeFocused({ timeout: 1500 });
  await page.keyboard.press('Tab');
  await expect.soft(form.locator('[name=email]')).toBeFocused({ timeout: 1500 });
  await expect(form.locator('[name=current_password]')).toHaveValue('');
  await form.locator('[name=email]').fill(email);
  await form.locator('[name=current_password]').fill(process.env.A11Y_PASSWORD);
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toBeVisible();
  await expect(form.locator('[name=email]')).toHaveValue(email);
  // Exercise the second server-side refusal with a known occupied address.
  await form.locator('[name=email]').fill('developer@localhost');
  await form.locator('[name=current_password]').fill(process.env.A11Y_PASSWORD);
  await form.locator('button[type=submit]').press('Enter');
  await expect(form.locator('[name=email]')).toHaveAttribute('aria-invalid','true');
  await expect.soft(form.locator('[name=email]')).toHaveValue('developer@localhost', { timeout: 1500 });
  await expect.soft(page.locator('#form-error')).toBeFocused({ timeout: 1500 });
  await form.locator('[name=email]').fill(original);
  await form.locator('[name=current_password]').fill(process.env.A11Y_PASSWORD);
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toBeVisible();
}

test('account email errors preserve the proposed address and allow correction', async ({ page }, info) => emailErrors(page, info));
test('@no-js account email errors preserve the proposed address and allow correction', async ({ page }, info) => emailErrors(page, info));

test('profile and reading preferences can be edited and restored with the keyboard', async ({ page }, info) => {
  await signIn(page);
  await page.goto('/settings/profile');
  const profile = page.locator('form[action="/web/settings/profile"]');
  const name = await profile.locator('[name=display_name]').inputValue();
  const note = await profile.locator('[name=note]').inputValue();
  await profile.locator('[name=display_name]').fill('Accessible profile');
  await profile.locator('[name=note]').fill('A profile edited using keyboard controls.');
  await profile.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toContainText('Profile saved');
  await expect(profile.locator('[name=display_name]')).toHaveValue('Accessible profile');
  await audit(page, info, 'profile-saved');
  await profile.locator('[name=display_name]').fill(name);
  await profile.locator('[name=note]').fill(note);
  await profile.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toBeVisible();
  await page.goto('/settings/preferences');
  const preferences = page.locator('form[action="/web/settings/preferences"]');
  const zone = await preferences.locator('[name=time_zone]').inputValue();
  const expanded = await preferences.locator('[type=checkbox][name=reading_expand_spoilers]').isChecked();
  await preferences.locator('[name=time_zone]').selectOption('Asia/Tbilisi');
  await preferences.locator('[type=checkbox][name=reading_expand_spoilers]').setChecked(!expanded);
  await preferences.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toContainText('Preferences saved');
  await expect(preferences.locator('[name=time_zone]')).toHaveValue('Asia/Tbilisi');
  await audit(page, info, 'preferences-saved');
  await preferences.locator('[name=time_zone]').selectOption(zone);
  await preferences.locator('[type=checkbox][name=reading_expand_spoilers]').setChecked(expanded);
  await preferences.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toBeVisible();
});
