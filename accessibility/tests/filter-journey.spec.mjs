import { test, expect } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import { signIn } from '../session.mjs';

async function filterJourney(page, info) {
  await signIn(page);
  await page.goto('/settings/filters/new');
  const form = page.locator('form[action="/web/settings/filters"]');
  const title = `Quiet words ${info.project.name}`;
  const fill = async () => {
    await form.locator('[name=title]').fill(title);
    await form.locator('[name=action]').selectOption('hide');
    await form.locator('[name=expires_in]').selectOption('86400');
    await form.locator('[name=new_keyword]').fill('quietword');
    await form.locator('[type=checkbox][name=new_whole_word]').check();
  };
  await fill();
  // Context selection is already required by the server; leave it empty to recover.
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.locator('#form-error')).toContainText('Pick at least one context');
  await page.screenshot({ path: info.outputPath('filter-create-error.png'), fullPage: true });
  await expect.soft(page.locator('#form-error')).toBeFocused({ timeout: 1500 });
  await expect.soft(form.locator('[name=title]')).toHaveValue(title, { timeout: 1500 });
  await expect.soft(form.locator('[name=action]')).toHaveValue('hide', { timeout: 1500 });
  await expect.soft(form.locator('[name=expires_in]')).toHaveValue('86400', { timeout: 1500 });
  await expect.soft(form.locator('[name=new_keyword]')).toHaveValue('quietword', { timeout: 1500 });
  await expect.soft(form.locator('[type=checkbox][name=new_whole_word]')).toBeChecked({ timeout: 1500 });
  if (info.project.use.javaScriptEnabled !== false) {
    const result = await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
    expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
  await fill(); // Keep the baseline journey able to finish after soft assertions.
  await form.locator('[name=context][value=home]').check();
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toContainText('Filter saved');
  const edit = page.locator('form[action^="/web/settings/filters/"]').first();
  await expect(edit.locator('[name=title]')).toHaveValue(title);
  await expect(edit.locator('[name=action]')).toHaveValue('hide');
  await expect(edit.locator('[name=expires_in]')).toHaveValue('keep');
  await expect(edit.locator('[name=context][value=home]')).toBeChecked();
  await expect(edit.locator('input[name^="keywords["][type=text]')).toHaveValue('quietword');
  await edit.locator('[name=title]').fill(`${title} edited`);
  await edit.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toBeVisible();
  await page.goto('/settings/filters');
  await expect(page.getByRole('link', {name: `${title} edited`, exact:true})).toBeVisible();
}

test('filter creation retains its settings after missing context and can be corrected', async ({page}, info) => filterJourney(page, info));
test('@no-js filter creation retains its settings after missing context and can be corrected', async ({page}, info) => filterJourney(page, info));
