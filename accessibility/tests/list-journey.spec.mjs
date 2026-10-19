import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {signIn} from '../session.mjs';

async function audit(page,info,name) {
  await page.screenshot({path:info.outputPath(`${name}.png`),fullPage:true});
  if (info.project.use.javaScriptEnabled === false) return;
  const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
  expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
}

async function listJourney(page,info) {
  await signIn(page);
  await page.goto('/lists/new');
  const create=page.locator('form[action="/web/lists"]');
  await create.locator('[name=title]').fill('   ');
  await create.locator('[name=replies_policy]').selectOption('none');
  await create.locator('[type=checkbox][name=exclusive]').check();
  await create.locator('button[type=submit]').press('Enter');
  await expect(page.locator('#form-error')).toContainText("Title can't be blank");
  await audit(page,info,'list-create-error');
  await expect.soft(page.locator('#form-error')).toBeFocused({timeout:1500});
  await expect.soft(create.locator('[name=replies_policy]')).toHaveValue('none',{timeout:1500});
  await expect.soft(create.locator('[type=checkbox][name=exclusive]')).toBeChecked({timeout:1500});
  await create.locator('[name=title]').fill(`Accessible list ${info.project.name}`);
  await create.locator('[name=replies_policy]').selectOption('none');
  await create.locator('[type=checkbox][name=exclusive]').check();
  await create.locator('button[type=submit]').press('Enter');
  await page.waitForURL(url=>/^\/lists\/\d+$/.test(url.pathname));
  const listPath=new URL(page.url()).pathname;
  await page.goto(`${listPath}/members`);
  const add=page.locator('form[action$="/members/add"]');
  await add.locator('[name=handle]').fill('developer');
  await add.locator('button[type=submit]').press('Enter');
  await expect(page.locator('#form-error')).toContainText('You can only add accounts you follow');
  await audit(page,info,'list-member-error');
  await expect.soft(page.locator('#form-error')).toBeFocused({timeout:1500});
  await expect.soft(add.locator('[name=handle]')).toHaveValue('developer',{timeout:1500});
  await page.goto('/@developer');
  await page.locator('form[action$="/follow"] button').first().press('Enter');
  await expect(page.locator('form[action$="/unfollow"]')).toHaveCount(1);
  await page.goto(`${listPath}/members`);
  await add.locator('[name=handle]').fill('developer');
  await add.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.relationships-list__item a[href="/@developer"]').first()).toBeVisible();
  await page.goto(listPath);
  await expect(page.locator('article.status').first()).toBeVisible();
  await page.goto(`${listPath}/members`);
  await page.locator('form[action$="/members/remove"] button').press('Enter');
  await expect(page.locator('.empty')).toContainText('no members');
  await page.goto(`${listPath}/edit`);
  const edit=page.locator('form[action$="/edit"]');
  await expect(edit.locator('[name=replies_policy]')).toHaveValue('none');
  await expect(edit.locator('[type=checkbox][name=exclusive]')).toBeChecked();
  await edit.locator('[name=title]').fill('Renamed fixture list');
  await edit.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toBeVisible();
  if (info.project.use.javaScriptEnabled !== false) page.once('dialog',dialog=>dialog.accept());
  await page.locator('form[data-confirm-action$="/delete"] button').press('Enter');
  if (info.project.use.javaScriptEnabled === false) {
    await page.locator('form[action$="/delete"] button').press('Enter');
  }
  await expect(page).toHaveURL(/\/lists\?saved=1$/);
  await expect(page.getByRole('link',{name:'Renamed fixture list',exact:true})).toHaveCount(0);
  await page.goto('/@developer');
  await page.locator('form[action$="/unfollow"] button').first().press('Enter');
  await expect(page.locator('form[action$="/follow"]')).toHaveCount(1);
}

test('list creation and member errors preserve work through follow, edit and delete',async({page},info)=>listJourney(page,info));
test('@no-js list creation and member errors preserve work through native confirmation',async({page},info)=>listJourney(page,info));
