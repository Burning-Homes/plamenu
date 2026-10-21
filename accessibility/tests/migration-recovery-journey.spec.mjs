import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {signIn} from '../session.mjs';

async function journey(page,info) {
  test.setTimeout(120000);
  if(info.project.use.javaScriptEnabled===false) await page.setViewportSize({width:320,height:800});
  await signIn(page);
  await page.goto('/settings/aliases');
  let form=page.locator('form[action="/web/settings/aliases"]');
  await form.locator('[name=acct]').fill('needs-a-domain');
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.getByRole('alert')).toContainText('not a valid handle');
  await page.screenshot({path:info.outputPath('alias-error.png'),fullPage:true});
  await expect.soft(page.locator('#migration-error')).toBeFocused({timeout:1000});
  await expect.soft(form.locator('[name=acct]')).toHaveValue('needs-a-domain',{timeout:1000});
  if(info.project.use.javaScriptEnabled!==false) {
    const scan=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
    expect.soft(scan.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
  await page.goto('/settings/migration');
  form=page.locator('form[action="/web/settings/migration"]');
  await form.locator('[name=acct]').fill('new-account@example.test');
  await form.locator('[name=current_password]').fill('incorrect-fixture-password');
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.getByRole('alert')).toContainText('Incorrect password');
  await page.screenshot({path:info.outputPath('move-error.png'),fullPage:true});
  await expect.soft(page.locator('#migration-error')).toBeFocused({timeout:1000});
  await expect.soft(form.locator('[name=acct]')).toHaveValue('new-account@example.test',{timeout:1000});
  await expect(form.locator('[name=current_password]')).toHaveValue('');
  // Correct password, deliberately invalid handle: no remote lookup or account move.
  await form.locator('[name=acct]').fill('needs-a-domain');
  await form.locator('[name=current_password]').fill(process.env.A11Y_PASSWORD || 'plamenu-development-only');
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.getByRole('alert')).toContainText('not a valid handle');
  await expect.soft(form.locator('[name=acct]')).toHaveValue('needs-a-domain',{timeout:1000});
  await expect(form.locator('[name=current_password]')).toHaveValue('');
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=document.documentElement.clientWidth+1)).toBe(true);
}
test('Alias and migration refusals retain the handle and focus the explanation',async({page},info)=>journey(page,info));
test('@no-js alias and migration recovery uses native forms',async({page},info)=>journey(page,info));
