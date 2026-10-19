import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {signIn} from '../session.mjs';

async function menu(page) {
  const details=page.locator('details.profile__menu');
  if (await details.getAttribute('open')===null) await details.locator(':scope > summary').press('Enter');
  return details;
}

async function blockingJourney(page,info) {
  const native=info.project.use.javaScriptEnabled===false;
  await signIn(page);
  await page.goto('/@developer');
  await page.locator('form[action$="/follow"] button').first().press('Enter');
  await expect(page.locator('form[action$="/unfollow"]')).toHaveCount(1);
  let actions=await menu(page);
  if (!native) page.once('dialog',dialog=>dialog.dismiss());
  await actions.locator('form[data-confirm-action$="/block"] button').press('Enter');
  if (native) await page.locator('.confirmation-card').getByRole('link',{name:'Cancel',exact:true}).press('Enter');
  await expect(page.locator('form[action$="/unfollow"]')).toHaveCount(1);
  actions=await menu(page);
  if (!native) page.once('dialog',dialog=>dialog.accept());
  await actions.locator('form[data-confirm-action$="/block"] button').press('Enter');
  if (native) await page.locator('.confirmation-card form[action$="/block"] button').press('Enter');
  await expect(page.locator('.profile__moderation')).toContainText('Blocked by you');
  await page.goto('/settings/relationships?rel=blocked');
  const selection=page.locator('.relationships-list input[name=ids]');
  await expect(selection).toHaveCount(1);
  await expect.soft(selection).toHaveAccessibleName('Select @developer',{timeout:1500});
  await page.screenshot({path:info.outputPath('blocked-relationships.png'),fullPage:true});
  if (!native) {
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
    expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
  await selection.press('Space');
  await expect(selection).toBeChecked();
  await page.locator('form[action="/web/settings/relationships"] button[type=submit]').press('Enter');
  await expect(page.locator('.relationships-list input[name=ids]')).toHaveCount(0);
  await page.goto('/@developer');
  await expect(page.locator('.profile__moderation')).toHaveCount(0);
  // Unblocking must not silently recreate the relationship removed by blocking.
  await expect(page.locator('form[action$="/follow"]')).toHaveCount(1);
}

test('cancel, block, select and unblock preserve keyboard access and relationship intent',async({page},info)=>blockingJourney(page,info));
test('@no-js block cancellation and relationship recovery use native forms',async({page},info)=>blockingJourney(page,info));
