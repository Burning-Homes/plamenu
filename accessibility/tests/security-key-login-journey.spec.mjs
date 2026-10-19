import {test,expect} from '@playwright/test';
import {totp} from '../authenticator.mjs';
import AxeBuilder from '@axe-core/playwright';
import {useFixtureClient} from '../session.mjs';

async function logout(page) {
  if(!await page.locator('form[action="/logout"] button:visible').count()) await page.locator('details[data-drawer] > summary').press('Enter');
  await Promise.all([page.waitForURL(url=>url.pathname==='/login'),page.locator('form[action="/logout"] button:visible').press('Enter')]);
}

test('Existing security-key sign-in recovers focus after refusal and still authenticates',async({page,context,browserName},info)=>{
  test.skip(browserName!=='chromium','Chromium CDP virtual authenticator coverage only; physical prompts are unverified.');
  test.setTimeout(150000);
  await useFixtureClient(page,'security');
  const cdp=await context.newCDPSession(page);
  await cdp.send('WebAuthn.enable');
  const {authenticatorId}=await cdp.send('WebAuthn.addVirtualAuthenticator',{options:{protocol:'ctap2',ctap2Version:'ctap2_1',transport:'usb',hasResidentKey:true,hasUserVerification:true,isUserVerified:true,automaticPresenceSimulation:true}});
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
  // Leave the one-time recovery-code page before making any review screenshots.
  await page.goto('/settings/security');
  const register=page.locator('[data-webauthn-register-form]');
  const add=register.getByRole('button',{name:'Add security key',exact:true});
  await register.locator('[data-webauthn-nickname]').fill('Existing fixture key');
  // Fixture enrollment only: Chromium rejects the library's preferred-UV /
  // required-credProtect combination. Tighten this one fixture request so we
  // can exercise existing-key sign-in. No production policy is changed, and
  // this is explicitly NOT qualification of production key registration.
  await page.route('**/web/settings/webauthn/options',async route=>{
    const response=await route.fetch();
    expect(response.status()).toBe(200);
    const body=await response.json();
    body.options.publicKey.authenticatorSelection.userVerification='required';
    await route.fulfill({response,json:body});
  },{times:1});
  await Promise.all([page.waitForNavigation(),add.press('Enter')]);
  await expect(page.locator('.two-factor__key-name')).toHaveText('Existing fixture key');
  const nativeContext=await context.browser().newContext({...info.project.use,baseURL:process.env.A11Y_BASE_URL,javaScriptEnabled:false});
  const native=await nativeContext.newPage();
  try {
    await useFixtureClient(native,'security');
    await native.goto('/login');
    await native.locator('[name=identifier]').fill(`security_${info.project.name.replaceAll('-','_')}`);
    await native.locator('[name=password]').fill(process.env.A11Y_PASSWORD);
    await native.locator('form.auth-form button[type=submit]').press('Enter');
    const nativeChallenge=native.locator('form[action="/login/challenge"]');
    await expect(nativeChallenge).toBeVisible();
    await native.locator('.auth-card').screenshot({path:info.outputPath('key-native-login.png')});
    await expect.soft(native.locator('[data-webauthn-login]')).toBeHidden({timeout:1000});
    await nativeChallenge.locator('[name=code]').fill(recovery);
    await nativeChallenge.locator('button[type=submit]').press('Enter');
    await expect(native).toHaveURL(url=>url.pathname==='/');
  } finally {await nativeContext.close();}

  await logout(page);
  await credentials();
  const authenticate=page.locator('[data-webauthn-authenticate]');
  await expect(authenticate).toBeVisible();
  await page.route('**/login/webauthn/options',route=>route.fulfill({status:503,contentType:'application/json',body:JSON.stringify({error:'Fixture service unavailable'})}),{times:1});
  await authenticate.press('Enter');
  await expect(page.locator('[data-webauthn-login] [role=status]')).toContainText('Fixture service unavailable');
  await expect.soft(authenticate).toBeFocused({timeout:1000});
  await page.locator('.auth-card').screenshot({path:info.outputPath('key-login-error.png')});
  const scan=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
  expect.soft(scan.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  let arrived,release;
  const intercepted=new Promise(resolve=>{arrived=resolve;});
  const resumed=new Promise(resolve=>{release=resolve;});
  await page.route('**/login/webauthn/options',async route=>{
    arrived();await resumed;
    await route.fulfill({status:503,contentType:'application/json',body:JSON.stringify({error:'Fixture delayed failure'})});
  },{times:1});
  await authenticate.press('Enter');
  await intercepted;
  const fallbackCode=page.locator('form[action="/login/challenge"] [name=code]');
  await fallbackCode.fill('not submitted');
  release();
  await expect(page.locator('[data-webauthn-login] [role=status]')).toContainText('Fixture delayed failure');
  await expect(fallbackCode).toBeFocused();
  await expect(fallbackCode).toHaveValue('not submitted');
  await authenticate.press('Enter');
  await expect(page).toHaveURL(url=>url.pathname==='/');
  await page.goto('/settings/security');
  await page.locator('.two-factor__key').getByRole('button',{name:'Remove',exact:true}).press('Enter');
  await expect(page.locator('.two-factor__key')).toHaveCount(0);
  const disable=page.locator('form[action="/web/settings/two_factor/disable"]');
  await expect(disable).toBeVisible();
  await disable.locator('[name=current_password]').fill(process.env.A11Y_PASSWORD);
  await disable.locator('button[type=submit]').press('Enter');
  await expect(setup).toBeVisible();
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=document.documentElement.clientWidth+1)).toBe(true);
  await cdp.send('WebAuthn.removeVirtualAuthenticator',{authenticatorId});
});
