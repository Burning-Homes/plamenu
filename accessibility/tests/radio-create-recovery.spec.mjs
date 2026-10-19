import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {signIn,openDetailsFor} from '../session.mjs';

async function journey(page,info) {
  if(info.project.use.javaScriptEnabled===false) await page.setViewportSize({width:320,height:800});
  await signIn(page);
  await page.goto('/music/radios');
  const form=page.locator('form[action="/web/music/radios/create"]');
  const description='Retained station <description> & details';
  const config='Opaque station configuration; no new JSON requirement.';
  await openDetailsFor(form);
  async function draft(name) {
    await form.locator('[name=name]').fill(name);
    await form.locator('[name=description]').fill(description);
    await form.locator('[name=visibility]').selectOption('local');
    await form.locator('[name=config]').fill(config);
  }
  await draft('   ');
  const rejected=page.waitForResponse(r=>r.request().method()==='POST'&&new URL(r.url()).pathname==='/web/music/radios/create');
  await form.locator('button[type=submit]').press('Enter');
  expect((await rejected).status()).toBe(422);
  await page.screenshot({path:info.outputPath('radio-create-error.png'),fullPage:true});
  await expect.soft(page.locator('#radio-create-error')).toBeFocused({timeout:1000});
  await expect.soft(form.locator('[name=name]')).toHaveAttribute('aria-invalid','true',{timeout:1000});
  await expect.soft(form.locator('[name=description]')).toHaveValue(description,{timeout:1000});
  await expect.soft(form.locator('[name=config]')).toHaveValue(config,{timeout:1000});
  await expect.soft(form.locator('[name=visibility]')).toHaveValue('local',{timeout:1000});
  if(!await form.count()) {
    await page.goto('/music/radios');
    await openDetailsFor(form);
    await draft('Corrected station');
  } else {
    expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
    if(info.project.use.javaScriptEnabled!==false) {
      const scan=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
      expect.soft(scan.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
    }
    await form.locator('[name=name]').fill('Corrected station');
  }
  await form.locator('button[type=submit]').press('Enter');
  await expect(page).toHaveURL(url=>/^\/music\/radios\/\d+$/.test(url.pathname));
  await expect(page.getByRole('heading',{name:'Corrected station',exact:true})).toBeVisible();
  const edit=page.locator('form[action$="/edit"]');
  await openDetailsFor(edit);
  await expect(edit.locator('[name=description]')).toHaveValue(description);
  await expect(edit.locator('[name=config]')).toHaveValue(config);
  await expect(edit.locator('[name=visibility]')).toHaveValue('local');
}
test('radio creation retains station details after a blank-name rejection',async({page},info)=>journey(page,info));
test('@no-js radio creation retains station details after a blank-name rejection',async({page},info)=>journey(page,info));
