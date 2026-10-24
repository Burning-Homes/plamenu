import {test,expect} from '../fixtures.mjs';
import {totp} from '../authenticator.mjs';
import AxeBuilder from '@axe-core/playwright';
import {useFixtureClient} from '../session.mjs';

async function logout(page) {
  if(!await page.locator('form[action="/logout"] button:visible').count()) await page.locator('details[data-drawer] > summary').press('Enter');
  await Promise.all([page.waitForURL(url=>url.pathname==='/login'),page.locator('form[action="/logout"] button:visible').press('Enter')]);
}

async function journey(page,info) {
  test.setTimeout(150000);
  if(info.project.use.javaScriptEnabled===false) await page.setViewportSize({width:320,height:800});
  await useFixtureClient(page,'security');
  async function credentials() {
    await page.goto('/login');
    await page.locator('[name=identifier]').fill(`security_${info.project.name.replaceAll('-','_')}`);
    await page.locator('[name=password]').fill(process.env.A11Y_PASSWORD);
    await page.locator('form.auth-form button[type=submit]').press('Enter');
  }
  await credentials();
  await expect(page).toHaveURL(url=>url.pathname==='/');
  await page.goto('/settings/security');
  const setup=page.locator('form[action="/web/settings/two_factor/setup"]');
  await setup.locator('[name=current_password]').fill(process.env.A11Y_PASSWORD);
  await setup.locator('button[type=submit]').press('Enter');
  const secret=await page.locator('.two-factor__qr + p code').textContent();
  const confirm=page.locator('form[action="/web/settings/two_factor/confirm"]');
  await confirm.locator('[name=code]').fill(totp(secret));
  await confirm.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.two-factor__codes code')).toHaveCount(10);
  const recovery=await page.locator('.two-factor__codes code').first().textContent();
  await page.goto('/settings/security');
  const register=page.locator('[data-webauthn-register-form]');
  if(info.project.use.javaScriptEnabled!==false) {
    const add=register.getByRole('button',{name:'Add security key',exact:true});
    await add.press('Enter');
    await expect(register.getByRole('status')).toContainText('Give the security key a name first');
    await register.locator('[data-webauthn-nickname]').fill('Review fixture key');
    await page.route('**/web/settings/webauthn/options',route=>route.fulfill({status:503,contentType:'application/json',body:JSON.stringify({error:'Fixture service unavailable'})}));
    await add.press('Enter');
    await expect(register.getByRole('status')).toContainText('Fixture service unavailable');
    await expect.soft(add).toBeFocused({timeout:1000});
    await expect(register.locator('[data-webauthn-nickname]')).toHaveValue('Review fixture key');
    await register.locator('..').screenshot({path:info.outputPath('key-register-error.png')});
    // A delayed failure must not take focus back after the reader moves on.
    await page.unroute('**/web/settings/webauthn/options');
    let arrived,release;
    const intercepted=new Promise(resolve=>{arrived=resolve;});
    const resumed=new Promise(resolve=>{release=resolve;});
    await page.route('**/web/settings/webauthn/options',async route=>{
      arrived();await resumed;
      await route.fulfill({status:503,contentType:'application/json',body:JSON.stringify({error:'Fixture delayed failure'})});
    });
    await add.press('Enter');
    await intercepted;
    await register.locator('[data-webauthn-nickname]').focus();
    release();
    await expect(register.getByRole('status')).toContainText('Fixture delayed failure');
    await expect(register.locator('[data-webauthn-nickname]')).toBeFocused();
    const scan=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
    expect.soft(scan.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  } else {
    for(const [width,label] of [[1440,'desktop'],[320,'mobile']]) {
      await page.setViewportSize({width,height:800});
      await register.locator('..').screenshot({path:info.outputPath(`key-native-${label}.png`)});
    }
    await expect.soft(register).toBeHidden({timeout:1000});
    await expect(page.getByText('Adding a security key requires JavaScript.',{exact:true})).toBeVisible();
  }
  await expect(page.locator('.two-factor__key')).toHaveCount(0);
  await logout(page);
  await credentials();
  const challenge=page.locator('form[action="/login/challenge"]');
  await challenge.locator('[name=code]').fill(recovery);
  await challenge.locator('button[type=submit]').press('Enter');
  await expect(page).toHaveURL(url=>url.pathname==='/');
  await page.goto('/settings/security');
  const disable=page.locator('form[action="/web/settings/two_factor/disable"]');
  await disable.locator('[name=current_password]').fill(process.env.A11Y_PASSWORD);
  await disable.locator('button[type=submit]').press('Enter');
  await expect(setup).toBeVisible();
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=document.documentElement.clientWidth+1)).toBe(true);
}
test('Security-key registration failure retains focus and existing sign-in alternatives',async({page},info)=>journey(page,info));
test('@no-js security-key explanation preserves existing two-factor sign-in',async({page},info)=>journey(page,info));
