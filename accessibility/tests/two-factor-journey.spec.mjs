import {useFixtureClient} from '../session.mjs';
import {test,expect} from '@playwright/test';
import {createHmac} from 'node:crypto';
import AxeBuilder from '@axe-core/playwright';

test.beforeEach(async({page})=>useFixtureClient(page,'security'));

// Independent authenticator calculation from the setup key presented to the user.
function totp(secret) {
  const alphabet='ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
  let bits=0,value=0;
  const bytes=[];
  for (const char of secret.trim().toUpperCase()) {
    value=(value<<5)|alphabet.indexOf(char);
    bits+=5;
    if (bits>=8) { bits-=8; bytes.push((value>>>bits)&255); }
  }
  const counter=Buffer.alloc(8);
  counter.writeBigUInt64BE(BigInt(Math.floor(Date.now()/30000)));
  const digest=createHmac('sha1',Buffer.from(bytes)).update(counter).digest();
  const offset=digest[digest.length-1]&15;
  return String((digest.readUInt32BE(offset)&0x7fffffff)%1000000).padStart(6,'0');
}

async function logout(page) {
  if (!await page.locator('form[action="/logout"] button:visible').count()) await page.locator('details[data-drawer] > summary').press('Enter');
  await Promise.all([
    page.waitForURL(url=>url.pathname==='/login'),
    page.locator('form[action="/logout"] button:visible').press('Enter'),
  ]);
}

async function journey(page,info) {
  const username=`security_${info.project.name.replaceAll('-','_')}`;
  async function credentials() {
    await page.goto('/login');
    await page.locator('[name=identifier]').fill(username);
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
  const recovery=await page.locator('.two-factor__codes code').allTextContents();
  // Setup keys and recovery codes are never captured in committed screenshots.
  await logout(page);
  await credentials();
  const challenge=page.locator('form[action="/login/challenge"]');
  await expect(challenge).toBeVisible();
  await challenge.locator('[name=code]').fill('invalid');
  await challenge.locator('button[type=submit]').press('Enter');
  await expect(page.locator('#challenge-error')).toBeVisible();
  await page.screenshot({path:info.outputPath('two-factor-login-error.png'),fullPage:true});
  await expect.soft(page.locator('#challenge-error')).toBeFocused({timeout:1500});
  await expect(challenge.locator('[name=code]')).toHaveValue('');
  await expect(challenge.locator('[name=code]')).toHaveAttribute('aria-describedby','challenge-error');
  if (info.project.use.javaScriptEnabled!==false) {
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
    expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
  await challenge.locator('[name=code]').fill(recovery[0]);
  await challenge.locator('button[type=submit]').press('Enter');
  await expect(page).toHaveURL(url=>url.pathname==='/');
  await logout(page);
  await credentials();
  await expect(challenge).toBeVisible();
  await challenge.locator('[name=code]').fill(recovery[0]);
  await challenge.locator('button[type=submit]').press('Enter');
  await expect(page.locator('#challenge-error')).toBeVisible();
  await expect.soft(page.locator('#challenge-error')).toBeFocused({timeout:1500});
  await challenge.locator('[name=code]').fill(recovery[1]);
  await challenge.locator('button[type=submit]').press('Enter');
  await expect(page).toHaveURL(url=>url.pathname==='/');
  await page.goto('/settings/security');
  const disable=page.locator('form[action="/web/settings/two_factor/disable"]');
  await disable.locator('[name=current_password]').fill(process.env.A11Y_PASSWORD);
  await disable.locator('button[type=submit]').press('Enter');
  await expect(setup).toBeVisible();
}

test('opt-in two-factor enrollment and recovery-code correction remain accessible',async({page},info)=>journey(page,info));
test('@no-js opt-in two-factor enrollment and recovery-code correction remain accessible',async({page},info)=>journey(page,info));
