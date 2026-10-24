import {test,expect} from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import {useFixtureClient} from '../session.mjs';

test.beforeEach(async({page})=>useFixtureClient(page,'throttle'));

async function submit(page,password) {
  await page.locator('[name=password]').fill(password);
  const [response]=await Promise.all([
    page.waitForNavigation({waitUntil:'domcontentloaded'}),
    page.locator('form.auth-form button[type=submit]').press('Enter'),
  ]);
  return response;
}

async function throttleJourney(page,info) {
  test.setTimeout(180000);
  if(info.project.use.javaScriptEnabled===false) await page.setViewportSize({width:320,height:800});
  const username=`limit_${info.project.name.replaceAll('-','_')}`;
  await page.goto('/login');
  await page.locator('[name=identifier]').fill(username);
  let response;
  // Exercise the ordinary production threshold; do not disable or raise it.
  // Allow a fixed-window boundary to fall inside the sequence.
  for(let attempt=0;attempt<61;attempt++) {
    response=await submit(page,'incorrect fixture password');
    if(response.status()===429) break;
    expect(response.status()).toBe(401);
    await expect(page.locator('[name=identifier]')).toHaveValue(username);
  }
  expect(response.status()).toBe(429);
  await page.screenshot({path:info.outputPath('login-rate-limit.png'),fullPage:true});
  await expect.soft(page.locator('#login-rate-limit-error')).toBeFocused({timeout:1500});
  await expect.soft(page.getByRole('heading',{name:'Too many sign-in attempts',exact:true})).toBeVisible({timeout:1500});
  const back=page.getByRole('link',{name:'Return to sign in',exact:true});
  await expect.soft(back).toBeVisible({timeout:1500});
  if(await back.count()) {
    if(info.project.use.javaScriptEnabled!==false) {
      const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
      expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
    }
    await back.press('Enter');
    await expect(page.locator('form.auth-form')).toBeVisible();
    await page.locator('[name=identifier]').fill(username);
    // Returning to the form does not bypass the still-active admission limit.
    const retry=await submit(page,process.env.A11Y_PASSWORD);
    expect(retry.status()).toBe(429);
    await expect(page.locator('#login-rate-limit-error')).toBeFocused();
  }
}

test('browser login throttling explains the limit and offers a native return path',async({page},info)=>throttleJourney(page,info));
test('@no-js browser login throttling retains a native recovery path',async({page},info)=>throttleJourney(page,info));
