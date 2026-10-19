import {useFixtureClient} from '../session.mjs';
import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

test.beforeEach(async({page})=>useFixtureClient(page,'security'));
import {totp} from '../authenticator.mjs';

// Crop only the existing error and its focus outline. The setup key and QR
// remain outside the screenshot, even in the narrow layout.
async function errorEvidence(page,info,name) {
  const error=page.locator('#form-error');
  await error.scrollIntoViewIfNeeded();
  const box=await error.boundingBox(),viewport=page.viewportSize();
  const x=Math.max(0,box.x-6),y=Math.max(0,box.y-6);
  await page.screenshot({path:info.outputPath(`${name}.png`),clip:{x,y,width:Math.min(viewport.width-x,box.width+12),height:Math.min(viewport.height-y,box.height+12)}});
  await expect.soft(error).toBeFocused({timeout:1500});
}

async function setupJourney(page,info) {
  await page.goto('/login');
  await page.locator('[name=identifier]').fill(`security_${info.project.name.replaceAll('-','_')}`);
  await page.locator('[name=password]').fill(process.env.A11Y_PASSWORD);
  await page.locator('form.auth-form button[type=submit]').press('Enter');
  await expect(page).toHaveURL(url=>url.pathname==='/');
  await page.goto('/settings/security');
  const setup=page.locator('form[action="/web/settings/two_factor/setup"]');
  await setup.locator('[name=current_password]').fill('incorrect fixture password');
  await setup.locator('button[type=submit]').press('Enter');
  await expect(page.locator('#form-error')).toContainText('current password was incorrect');
  await errorEvidence(page,info,'two-factor-setup-password');
  await setup.locator('[name=current_password]').fill(process.env.A11Y_PASSWORD);
  await setup.locator('button[type=submit]').press('Enter');
  const confirm=page.locator('form[action="/web/settings/two_factor/confirm"]');
  await expect(confirm.locator('[name=code]')).toBeFocused();
  const secret=await page.locator('.two-factor__qr + p code').textContent();
  await confirm.locator('[name=code]').fill('invalid');
  await confirm.locator('button[type=submit]').press('Enter');
  await expect(page.locator('#form-error')).toContainText('That code was incorrect');
  await errorEvidence(page,info,'two-factor-setup-code');
  await expect(confirm.locator('[name=code]')).toHaveValue('');
  await expect.soft(confirm.locator('[name=code]')).toHaveAttribute('aria-invalid','true',{timeout:1500});
  await expect.soft(confirm.locator('[name=code]')).toHaveAttribute('aria-describedby','form-error',{timeout:1500});
  await expect(page.locator('form[action="/web/settings/password"] [aria-invalid=true]')).toHaveCount(0);
  expect(await page.locator('.two-factor__qr + p code').textContent()).toBe(secret);
  if (info.project.use.javaScriptEnabled!==false) {
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
    expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
  await confirm.locator('[name=code]').fill(totp(secret));
  await confirm.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.two-factor__codes code')).toHaveCount(10);
  await page.goto('/settings/security');
  const disable=page.locator('form[action="/web/settings/two_factor/disable"]');
  await disable.locator('[name=current_password]').fill('incorrect fixture password');
  await disable.locator('button[type=submit]').press('Enter');
  await expect(page.locator('#form-error')).toContainText('current password was incorrect');
  await expect.soft(page.locator('#form-error')).toBeFocused({timeout:1500});
  await expect(disable).toBeVisible();
  await disable.locator('[name=current_password]').fill(process.env.A11Y_PASSWORD);
  await disable.locator('button[type=submit]').press('Enter');
  await expect(setup).toBeVisible();
}

test('two-factor setup errors preserve the key and identify code correction',async({page},info)=>setupJourney(page,info));
test('@no-js two-factor setup errors support native correction',async({page},info)=>setupJourney(page,info));
