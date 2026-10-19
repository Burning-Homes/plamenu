import {test,expect} from '@playwright/test';
import {createHash,randomBytes} from 'node:crypto';
import AxeBuilder from '@axe-core/playwright';
import {totp} from '../authenticator.mjs';
import {useFixtureClient} from '../session.mjs';

test.beforeEach(async({page})=>useFixtureClient(page,'security'));

async function oauthJourney(page,browser,info) {
  test.setTimeout(120000);
  const username=`security_${info.project.name.replaceAll('-','_')}`;
  await page.goto('/login');
  await page.locator('[name=identifier]').fill(username);
  await page.locator('[name=password]').fill(process.env.A11Y_PASSWORD);
  await page.locator('form.auth-form button[type=submit]').press('Enter');
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
  await page.goto('/settings/applications/new');
  const app=page.locator('form[action="/web/settings/applications"]');
  const callback=new URL('/a11y-oauth-callback',process.env.A11Y_BASE_URL).href;
  await app.locator('[name=name]').fill('Disposable accessibility client');
  await app.locator('[name=redirect_uris]').fill(callback);
  await app.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toContainText('Application created');
  const clientId=await page.getByRole('row').filter({has:page.getByRole('rowheader',{name:'Client key',exact:true})}).locator('code').textContent();
  const clientSecret=await page.getByRole('row').filter({has:page.getByRole('rowheader',{name:'Client secret',exact:true})}).locator('code').textContent();
  const verifier=randomBytes(32).toString('base64url');
  const state=`fixture-${info.project.name}`;
  const authorize='/oauth/authorize?'+new URLSearchParams({response_type:'code',client_id:clientId,redirect_uri:callback,scope:'profile',state,code_challenge:createHash('sha256').update(verifier).digest('base64url'),code_challenge_method:'S256'});
  const guestContext=await browser.newContext({...info.project.use,baseURL:process.env.A11Y_BASE_URL});
  const guest=await guestContext.newPage();
  try {
    await guest.goto(authorize);
    const credentials=guest.locator('form.auth-form');
    await credentials.locator('[name=identifier]').fill(username);
    await credentials.locator('[name=password]').fill(process.env.A11Y_PASSWORD);
    await credentials.locator('button[type=submit]').press('Enter');
    const challenge=guest.locator('form.auth-form[action="/oauth/authorize"]');
    await expect(challenge.locator('[name=code]')).toBeFocused();
    await challenge.locator('[name=code]').fill('invalid');
    await challenge.locator('button[type=submit]').press('Enter');
    const error=guest.locator('#oauth-challenge-error');
    await expect(error).toBeVisible();
    await guest.screenshot({path:info.outputPath('oauth-challenge-error.png'),fullPage:true});
    await expect.soft(error).toBeFocused({timeout:1500});
    await expect(challenge.locator('[name=code]')).toHaveValue('');
    await expect(challenge.locator('[name=code]')).toHaveAttribute('aria-describedby','oauth-challenge-error');
    if (info.project.use.javaScriptEnabled!==false) {
      const result=await new AxeBuilder({page:guest}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
      expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
    }
    await challenge.locator('[name=code]').fill(recovery);
    await challenge.locator('button[type=submit]').press('Enter');
    await expect(guest).toHaveURL(url=>url.pathname==='/a11y-oauth-callback'&&url.searchParams.get('state')===state&&url.searchParams.has('code'));
    const grant=new URL(guest.url()).searchParams.get('code');
    // This is the disposable client's protocol step after the browser returns.
    // Tokens, setup keys and recovery codes are never logged or saved as evidence.
    const response=await guest.request.post('/oauth/token',{form:{grant_type:'authorization_code',client_id:clientId,client_secret:clientSecret,redirect_uri:callback,code:grant,code_verifier:verifier}});
    expect(response.ok()).toBe(true);
    const token=(await response.json()).access_token;
    const profile=await guest.request.get('/api/v1/accounts/verify_credentials',{headers:{Authorization:`Bearer ${token}`}});
    expect(profile.ok()).toBe(true);
    expect((await profile.json()).username).toBe(username);
    // A repeated request reuses the authenticated session but still offers consent.
    await guest.goto(authorize);
    await guest.locator('form').filter({has:guest.locator('[name=decision][value=deny]')}).getByRole('button').press('Enter');
    await expect(guest).toHaveURL(url=>url.pathname==='/a11y-oauth-callback'&&url.searchParams.get('state')===state&&url.searchParams.get('error')==='access_denied');
    await guest.goto(authorize);
    await guest.locator('form').filter({has:guest.locator('[name=decision][value=allow]')}).getByRole('button').press('Enter');
    await expect(guest).toHaveURL(url=>url.pathname==='/a11y-oauth-callback'&&url.searchParams.get('state')===state&&url.searchParams.has('code'));
    await page.goto('/settings/security');
    const disable=page.locator('form[action="/web/settings/two_factor/disable"]');
    await disable.locator('[name=current_password]').fill(process.env.A11Y_PASSWORD);
    await disable.locator('button[type=submit]').press('Enter');
    await expect(setup).toBeVisible();
  } finally {await guestContext.close();}
}

test('OAuth second-factor recovery preserves authorization and explicit consent',async({page,browser},info)=>oauthJourney(page,browser,info));
test('@no-js OAuth recovery and consent use native forms',async({page,browser},info)=>oauthJourney(page,browser,info));
