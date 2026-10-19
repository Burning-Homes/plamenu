import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {signIn} from '../session.mjs';

async function privacyJourney(page,info) {
  await signIn(page);
  await page.goto('/settings/privacy');
  const form=page.locator('form[action="/web/settings/privacy"]');
  const checkboxes=['discoverable','indexable','hide_collections','noindex','show_featured','show_media','show_media_replies'];
  const policies=['not_following_policy','not_followers_policy','new_accounts_policy','private_mentions_policy','limited_accounts_policy','bots_policy'];
  const original={};
  for (const name of checkboxes) original[name]=await form.locator(`[type=checkbox][name=${name}]`).isChecked();
  for (const name of policies) original[name]=await form.locator(`[name=${name}]`).inputValue();
  original.domains=await form.locator('[name=attribution_domains]').inputValue();
  async function draft(domains) {
    for (const name of checkboxes) await form.locator(`[type=checkbox][name=${name}]`).setChecked(!original[name]);
    for (const name of policies) await form.locator(`[name=${name}]`).selectOption(original[name]==='filter'?'drop':'filter');
    await form.locator('[name=attribution_domains]').fill(domains);
  }
  const invalid='https://valid.example.test/\nnot a domain';
  await draft(invalid);
  await form.locator('button[type=submit]').press('Enter');
  await expect.soft(form).toHaveCount(1,{timeout:1500});
  await page.screenshot({path:info.outputPath('privacy-domain-error.png'),fullPage:true});
  if (await form.count()) {
    await expect(page.locator('#form-error')).toBeFocused();
    await expect(form.locator('[name=attribution_domains]')).toHaveValue(invalid);
    await expect(form.locator('[name=attribution_domains]')).toHaveAttribute('aria-invalid','true');
    for (const name of checkboxes) await expect(form.locator(`[type=checkbox][name=${name}]`)).toBeChecked({checked:!original[name]});
    for (const name of policies) await expect(form.locator(`[name=${name}]`)).toHaveValue(original[name]==='filter'?'drop':'filter');
    if (info.project.use.javaScriptEnabled!==false) {
      const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
      expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
    }
  } else await page.goto('/settings/privacy');
  await draft('https://valid.example.test/');
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toBeVisible();
  await page.reload();
  await expect(form.locator('[name=attribution_domains]')).toHaveValue('valid.example.test');
  for (const name of checkboxes) {
    await expect(form.locator(`[type=checkbox][name=${name}]`)).toBeChecked({checked:!original[name]});
    await form.locator(`[type=checkbox][name=${name}]`).setChecked(original[name]);
  }
  for (const name of policies) {
    await expect(form.locator(`[name=${name}]`)).toHaveValue(original[name]==='filter'?'drop':'filter');
    await form.locator(`[name=${name}]`).selectOption(original[name]);
  }
  await form.locator('[name=attribution_domains]').fill(original.domains);
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toBeVisible();
}

test('privacy domain errors retain choices through correction and restoration',async({page},info)=>privacyJourney(page,info));
test('@no-js privacy domain errors retain choices through correction and restoration',async({page},info)=>privacyJourney(page,info));
