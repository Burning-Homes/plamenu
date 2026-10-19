import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {signIn,openDetailsFor,ensureVideoFormChannel} from '../session.mjs';

async function journey(page,noScript,info) {
  if(noScript) await page.setViewportSize({width:320,height:800});
  await signIn(page);
  await page.goto('/videos/import');
  const form=page.locator('form[action="/web/videos/import"]');
  await ensureVideoFormChannel(page,form);
  const title=form.getByRole('textbox',{name:'Video title',exact:true});
  const url=form.getByRole('textbox',{name:'Public source URL',exact:true});
  await expect(form.getByRole('combobox',{name:'Visibility',exact:true})).toBeVisible();
  await expect(page.locator('#video-import-privacy')).toBeVisible();
  await expect(form.getByLabel('Description',{exact:true})).toBeHidden();
  await url.fill('http://127.0.0.1/private.mp4');
  await title.fill('Keep my import title');
  await openDetailsFor(form.getByLabel('Description',{exact:true}));
  await form.getByLabel('Description',{exact:true}).fill('Keep my optional description');
  await form.getByLabel('Visual description',{exact:true}).fill('Keep my visual alternative');
  await form.getByRole('checkbox',{name:/I have permission to copy and publish/}).check();
  // The default private-address guard refuses before allocating or fetching.
  const posted=page.waitForResponse(r=>r.request().method()==='POST'&&new URL(r.url()).pathname==='/web/videos/import');
  await form.getByRole('button',{name:'Import into a private draft',exact:true}).press('Enter');
  expect((await posted).status()).toBe(422);
  await expect(page).toHaveURL(url=>url.pathname==='/web/videos/import');
  await expect(page.getByRole('alert')).toBeVisible();
  await expect.soft(page.getByRole('alert')).toBeFocused({timeout:1000});
  await expect(title).toHaveValue('Keep my import title');
  await expect(url).toHaveValue('http://127.0.0.1/private.mp4');
  await expect(form.getByLabel('Description',{exact:true})).toHaveValue('Keep my optional description');
  await expect(form.getByLabel('Visual description',{exact:true})).toHaveValue('Keep my visual alternative');
  expect(await page.locator('main').innerText()).not.toContain('studio-publishing-options');
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=document.documentElement.clientWidth)).toBe(true);
  if(!noScript) {
    const audit=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
    expect(audit.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
  await page.screenshot({path:info.outputPath('import-error-retains-options.png'),fullPage:true});
  await page.locator('.video-workspace__nav').locator('a[href="/videos"]').press('Enter');
  await expect(page).toHaveURL(url=>url.pathname==='/videos');
}
test('Import hierarchy and server refusal retain optional accessibility choices',async({page},info)=>journey(page,false,info));
test('Import refusal retains choices without scripts @no-js',async({page},info)=>journey(page,true,info));
