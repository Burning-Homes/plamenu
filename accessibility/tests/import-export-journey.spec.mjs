import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {readFile} from 'node:fs/promises';
import {signIn} from '../session.mjs';

async function journey(page,info) {
  test.setTimeout(180000);
  if(info.project.use.javaScriptEnabled===false) await page.setViewportSize({width:320,height:800});
  await signIn(page);
  await page.goto('/settings/export');
  const form=page.locator('form[action="/web/settings/import"]');
  await form.locator('[name=type]').selectOption('domain_blocking');
  await form.locator('[name=mode][value=overwrite]').press('Space');
  await form.locator('[name=data]').setInputFiles({name:'blocked_domains.csv',mimeType:'text/csv',buffer:Buffer.alloc(0)});
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.getByRole('alert')).toContainText('no rows to import');
  await page.screenshot({path:info.outputPath('import-error.png'),fullPage:true});
  await expect.soft(page.locator('#import-error')).toBeFocused({timeout:1000});
  await expect.soft(form.locator('[name=type]')).toHaveValue('domain_blocking',{timeout:1000});
  await expect.soft(form.locator('[name=mode][value=overwrite]')).toBeChecked({timeout:1000});
  if(info.project.use.javaScriptEnabled!==false) {
    const scan=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
    expect.soft(scan.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
  // Baseline still completes correction after recording its lost choices.
  if(await form.locator('[name=type]').inputValue()!=='domain_blocking') await form.locator('[name=type]').selectOption('domain_blocking');
  if(!await form.locator('[name=mode][value=overwrite]').isChecked()) await form.locator('[name=mode][value=overwrite]').press('Space');
  const domain=`import-${info.project.name}.example`;
  await form.locator('[name=data]').setInputFiles({name:'blocked_domains.csv',mimeType:'text/csv',buffer:Buffer.from(domain+'\n')});
  await form.locator('button[type=submit]').press('Enter');
  await expect(page).toHaveURL(url=>/^\/settings\/import\/\d+$/.test(url.pathname));
  await expect(page.locator('.settings-summary')).toContainText('Domain blocking list');
  await expect(page.locator('.settings-summary')).toContainText('Overwrite');
  await page.getByRole('button',{name:'Confirm import',exact:true}).press('Enter');
  await expect(page).toHaveURL(url=>url.pathname==='/settings/export');
  await expect.poll(async()=>{
    await page.reload();
    return page.locator('.export-list__label').filter({hasText:'Domain blocking list ·'}).textContent();
  },{timeout:60000,intervals:[500,1000,2000]}).toContain('Finished');
  const downloaded=page.waitForEvent('download');
  await page.locator('a[download="blocked_domains.csv"]').press('Enter');
  const download=await downloaded;
  const path=info.outputPath('blocked_domains.csv');
  await download.saveAs(path);
  expect((await readFile(path,'utf8')).trim()).toBe(domain);
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=document.documentElement.clientWidth+1)).toBe(true);
}
test('CSV import errors retain choices through review, application and export',async({page},info)=>journey(page,info));
test('@no-js CSV import recovery, confirmation and export use native controls',async({page},info)=>journey(page,info));
