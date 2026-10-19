import {useFixtureClient} from '../session.mjs';
import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

test.beforeEach(async({page})=>useFixtureClient(page,'security'));

async function login(page,username,password) {
  await page.goto('/login');
  // A revoked session can still have a remembered account in the chooser.
  const remembered=page.locator('.account-chooser__switch button');
  if (await remembered.count()) await remembered.press('Enter');
  await page.locator('[name=identifier]').fill(username);
  await page.locator('[name=password]').fill(password);
  await page.locator('button[type=submit]').press('Enter');
  await expect(page).toHaveURL(url=>url.pathname==='/');
}

async function audit(page,info,name) {
  await page.screenshot({path:info.outputPath(`${name}.png`),fullPage:true});
  if (info.project.use.javaScriptEnabled===false) return;
  const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
  expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
}

async function passwordJourney(page,browser,info) {
  const username=`security_${info.project.name.replaceAll('-','_')}`;
  const original=process.env.A11Y_PASSWORD;
  const replacement='Updated-fixture-password-2026';
  await login(page,username,original);
  const oldContext=await browser.newContext({...info.project.use,baseURL:process.env.A11Y_BASE_URL});
  const oldPage=await oldContext.newPage();
  try {
    await login(oldPage,username,original);
    await page.goto('/settings/security');
    const form=page.locator('form[action="/web/settings/security/password"]');
    async function fill(current,password,confirmation=password) {
      await form.locator('[name=current_password]').fill(current);
      await form.locator('[name=new_password]').fill(password);
      await form.locator('[name=confirm_password]').fill(confirmation);
    }
    await fill('incorrect-fixture-password',replacement);
    await form.locator('button[type=submit]').press('Enter');
    await expect(page.locator('#form-error')).toBeVisible();
    await audit(page,info,'password-change-current-error');
    await expect.soft(page.locator('#form-error')).toBeFocused({timeout:1500});
    await expect.soft(form.locator('[name=current_password]')).toHaveAttribute('aria-invalid','true',{timeout:1500});
    for (const input of await form.locator('input[type=password]').all()) await expect(input).toHaveValue('');
    await fill(original,replacement,`${replacement}-mismatch`);
    await form.locator('button[type=submit]').press('Enter');
    await expect(page.locator('#form-error')).toBeVisible();
    await audit(page,info,'password-change-mismatch');
    await expect.soft(page.locator('#form-error')).toBeFocused({timeout:1500});
    await expect.soft(form.locator('[name=confirm_password]')).toHaveAttribute('aria-invalid','true',{timeout:1500});
    await fill(original,replacement);
    await form.locator('button[type=submit]').press('Enter');
    await expect(page.locator('.settings__saved').filter({hasText:/^Password changed\./})).toBeVisible();
    await oldPage.goto('/settings/security');
    await expect(oldPage).toHaveURL(/\/login$/);
    // Restore the dedicated fixture's password through the same public form.
    await fill(replacement,original);
    await form.locator('button[type=submit]').press('Enter');
    await expect(page.locator('.settings__saved').filter({hasText:/^Password changed\./})).toBeVisible();
    await login(oldPage,username,original);
  } finally { await oldContext.close(); }
}

test('password-change errors lead to correction and revoke other sessions',async({page,browser},info)=>passwordJourney(page,browser,info));
test('@no-js password-change errors lead to correction and revoke other sessions',async({page,browser},info)=>passwordJourney(page,browser,info));
