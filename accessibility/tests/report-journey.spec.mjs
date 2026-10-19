import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {signIn,chooseInterfaceLanguage} from '../session.mjs';

async function openMenu(page) {
  const menu=page.locator('details.profile__menu');
  if (await menu.getAttribute('open') === null) await menu.locator(':scope > summary').press('Enter');
  return menu;
}

async function reportJourney(page,info,russian) {
  await signIn(page);
  try {
    if (russian) await chooseInterfaceLanguage(page,'ru');
    await page.goto('/@developer');
    const menu=await openMenu(page);
    await menu.locator('a[href*="/report"]').press('Enter');
    await expect(page.locator('.report-form')).toBeVisible();
    await page.locator('[name=category][value=spam]').press('Space');
    await expect(page.locator('[name=category][value=spam]')).toBeChecked();
    await page.locator('[name=comment]').fill('Accessibility fixture report, only in a disposable database.');
    await page.screenshot({path:info.outputPath('report-form.png'),fullPage:true});
    if (russian) {
      await expect.soft(page.locator('main h1')).toContainText('Пожаловаться', {timeout:1500});
      await expect.soft(page.locator('.report-form button[type=submit]')).toHaveText('Отправить жалобу', {timeout:1500});
      const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
      expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
    }
    await page.locator('.report-form button[type=submit]').press('Enter');
    await expect(page).toHaveURL(/done=1/);
    await page.screenshot({path:info.outputPath('report-done.png'),fullPage:true});
    await expect.soft(page.locator('main h1')).toHaveText(russian?'Спасибо за жалобу':'Thanks for reporting',{timeout:1500});
    const mute=page.locator('.report-done__actions form[action$="/mute"]');
    const action=await mute.getAttribute('action');
    const posted=page.waitForResponse(response=>response.request().method()==='POST'
      && new URL(response.url()).pathname===action);
    await mute.locator('button').press('Enter');
    expect((await posted).status()).toBe(303);
    await page.goto('/@developer');
    const muted=await openMenu(page);
    await muted.locator('form[action$="/unmute"] button').press('Enter');
    const restored=await openMenu(page);
    await expect(restored.locator('form[action$="/mute"]')).toHaveCount(1);
  } finally {
    if (russian) await chooseInterfaceLanguage(page,'en');
  }
}

test('Russian account report and optional mute recovery use the interface language',async({page},info)=>reportJourney(page,info,true));
test('@no-js account report and mute recovery remain native forms',async({page},info)=>reportJourney(page,info,false));
