import { test, expect } from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import { signIn, chooseInterfaceLanguage } from '../session.mjs';

async function rejectedImage(page, info, locale = 'en-US') {
  await signIn(page);
  await page.goto('/settings/profile');
  const form = page.locator('form[action="/web/settings/profile"]');
  const name = `Draft ${info.project.name}`;
  const note = 'Keep this biography after a rejected image.';
  await form.locator('[name=display_name]').fill(name);
  await form.locator('[name=note]').fill(note);
  await form.locator('[name=avatar_description]').fill('A small green avatar');
  await form.locator('[name="fields_attributes[0][name]"]').fill('Website');
  await form.locator('[name="fields_attributes[0][value]"]').fill('https://example.test');
  await form.locator('[type=checkbox][name=locked]').check();
  await form.locator('[name=avatar]').setInputFiles({ name: 'broken.png', mimeType: 'image/png', buffer: Buffer.from('not an image') });
  await form.locator('button[type=submit]').press('Enter');
  await page.waitForLoadState();
  await page.screenshot({ path: info.outputPath('profile-upload-error.png'), fullPage: true });
  await expect.soft(form).toHaveCount(1, { timeout: 1500 });
  if (!await form.count()) return; // Preserve baseline evidence before the repair exists.
  const error = page.locator('#profile-errors');
  await expect(error).toBeFocused();
  await expect(error.locator('[lang=en]')).toContainText('File content type is not supported');
  await expect(page.locator('html')).toHaveAttribute('lang', locale);
  await expect(error.locator('[lang=en]')).toHaveCount(1);
  if (locale === 'ru') await expect(error).toContainText('Не удалось сохранить профиль');
  await page.keyboard.press('Tab');
  await expect(form.locator('[name=display_name]')).toBeFocused();
  await expect(form.locator('[name=display_name]')).toHaveValue(name);
  await expect(form.locator('[name=note]')).toHaveValue(note);
  await expect(form.locator('[name=avatar_description]')).toHaveValue('A small green avatar');
  await expect(form.locator('[name="fields_attributes[0][name]"]')).toHaveValue('Website');
  await expect(form.locator('[name="fields_attributes[0][value]"]')).toHaveValue('https://example.test');
  await expect(form.locator('[type=checkbox][name=locked]')).toBeChecked();
  // Axe needs JavaScript; the no-JS journey keeps native form/focus assertions.
  if (info.project.use.javaScriptEnabled !== false) {
    const result = await new AxeBuilder({ page }).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
    expect(result.violations.map(v => ({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
  // The failed upload is empty; save the retained text without choosing another image.
  await expect(form.locator('[name=avatar]')).toHaveValue('');
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toBeVisible();
  await expect(form.locator('[name=display_name]')).toHaveValue(name);
  await expect(form.locator('[name=note]')).toHaveValue(note);
  await expect(form.locator('[type=checkbox][name=locked]')).toBeChecked();
}

test('rejected profile image keeps text, descriptions and choices for correction', async ({page}, info) => rejectedImage(page, info));
test('@no-js rejected profile image keeps text, descriptions and choices for correction', async ({page}, info) => rejectedImage(page, info));

test('Russian profile recovery marks the language of validation details', async ({page}, info) => {
  await signIn(page);
  try {
    await chooseInterfaceLanguage(page, 'ru');
    await rejectedImage(page, info, 'ru');
  } finally {
    await chooseInterfaceLanguage(page, 'en');
  }
});
