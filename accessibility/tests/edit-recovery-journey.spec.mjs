import {test,expect} from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import {signIn} from '../session.mjs';

async function audit(page,info,name) {
  await page.screenshot({path:info.outputPath(`${name}.png`),fullPage:true});
  await expect.soft(page.locator('#form-error')).toBeFocused({timeout:1500});
  if (info.project.use.javaScriptEnabled===false) return;
  const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
  expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
}

async function listEdit(page,info) {
  await signIn(page);
  await page.goto('/lists/new');
  await page.locator('[name=title]').fill('Original fixture list');
  await page.locator('form[action="/web/lists"] button[type=submit]').press('Enter');
  await page.waitForURL(url=>/^\/lists\/\d+$/.test(url.pathname));
  const path=new URL(page.url()).pathname;
  await page.goto(`${path}/edit`);
  const form=page.locator('form[action$="/edit"]');
  async function draft(title) {
    await form.locator('[name=title]').fill(title);
    await form.locator('[name=replies_policy]').selectOption('none');
    await form.locator('[type=checkbox][name=exclusive]').check();
  }
  await draft('   ');
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.locator('#form-error')).toContainText("Title can't be blank");
  await audit(page,info,'list-edit-error');
  await expect.soft(form.locator('[name=title]')).toHaveValue('   ',{timeout:1500});
  await expect.soft(form.locator('[name=title]')).toHaveAttribute('aria-invalid','true',{timeout:1500});
  await expect.soft(form.locator('[name=replies_policy]')).toHaveValue('none',{timeout:1500});
  await expect.soft(form.locator('[type=checkbox][name=exclusive]')).toBeChecked({timeout:1500});
  await draft('Corrected fixture list');
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toBeVisible();
  await page.reload();
  await expect(form.locator('[name=title]')).toHaveValue('Corrected fixture list');
  await expect(form.locator('[name=replies_policy]')).toHaveValue('none');
  await expect(form.locator('[type=checkbox][name=exclusive]')).toBeChecked();
}

async function filterEdit(page,info) {
  await signIn(page);
  await page.goto('/settings/filters/new');
  const create=page.locator('form[action="/web/settings/filters"]');
  await create.locator('[name=title]').fill('Original fixture filter');
  await create.locator('[name=context][value=home]').check();
  await create.locator('[name=new_keyword]').fill('originalword');
  await create.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toBeVisible();
  const form=page.locator('form[action^="/web/settings/filters/"]').first();
  const keyword=form.locator('input[type=text][name^="keywords["]');
  const wholeWord=form.locator('input[type=checkbox][name^="keywords["]');
  async function draft() {
    await form.locator('[name=title]').fill('Changed fixture filter');
    await form.locator('[name=action]').selectOption('hide');
    await form.locator('[name=expires_in]').selectOption('86400');
    await keyword.fill('editedword');
    await wholeWord.check();
    await form.locator('[name=new_keyword]').fill('addedword');
    await form.locator('[type=checkbox][name=new_whole_word]').check();
  }
  await draft();
  await form.locator('[name=context][value=home]').uncheck();
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.locator('#form-error')).toContainText('Pick at least one context');
  await audit(page,info,'filter-edit-error');
  await expect.soft(form.locator('[name=title]')).toHaveValue('Changed fixture filter',{timeout:1500});
  await expect.soft(form.locator('[name=action]')).toHaveValue('hide',{timeout:1500});
  await expect.soft(form.locator('[name=expires_in]')).toHaveValue('86400',{timeout:1500});
  await expect.soft(form.locator('[name=context]:checked')).toHaveCount(0,{timeout:1500});
  await expect.soft(keyword).toHaveValue('editedword',{timeout:1500});
  await expect.soft(wholeWord).toBeChecked({timeout:1500});
  await expect.soft(form.locator('[name=new_keyword]')).toHaveValue('addedword',{timeout:1500});
  await expect.soft(form.locator('[type=checkbox][name=new_whole_word]')).toBeChecked({timeout:1500});
  await draft(); // Also finish the lifecycle on the pre-fix executable.
  for (const checkbox of await form.locator('[name=context]:checked').all()) await checkbox.uncheck();
  await form.locator('[name=context][value=notifications]').check();
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toBeVisible();
  await page.reload();
  await expect(form.locator('[name=title]')).toHaveValue('Changed fixture filter');
  await expect(form.locator('[name=action]')).toHaveValue('hide');
  await expect(form.locator('[name=expires_in]')).toHaveValue('keep');
  await expect(form.locator('[name=context][value=notifications]')).toBeChecked();
  await expect(keyword).toHaveCount(2);
  const words=await keyword.evaluateAll(inputs=>inputs.map(input=>input.value).sort());
  expect(words).toEqual(['addedword','editedword']);
  for (const checkbox of await wholeWord.all()) await expect(checkbox).toBeChecked();
}

test('existing list validation preserves edits through correction',async({page},info)=>listEdit(page,info));
test('@no-js existing list validation preserves edits through correction',async({page},info)=>listEdit(page,info));
test('existing filter validation preserves settings and keyword edits through correction',async({page},info)=>filterEdit(page,info));
test('@no-js existing filter validation preserves settings and keyword edits through correction',async({page},info)=>filterEdit(page,info));
