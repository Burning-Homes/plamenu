import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {signIn} from '../session.mjs';

async function open(page,name) {
  const summary=page.locator('summary').filter({hasText:name});
  await summary.press('Enter');
}
async function audit(page,noScript) {
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
  if(!noScript) {
    const a=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
    expect(a.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
}
async function journey(page,noScript,info) {
  test.setTimeout(120000);
  await signIn(page);
  await page.goto('/videos/upload');
  const form=page.locator('form[action="/web/videos"]');
  await expect(form.getByLabel('Video or audio file',{exact:true})).toBeVisible();
  await expect(form.getByRole('combobox',{name:'Visibility',exact:true})).toBeVisible();
  await expect(form.getByLabel('Video title',{exact:true})).toHaveAttribute('required','');
  await expect(form.getByLabel('Visual description',{exact:true})).not.toBeVisible();
  await open(page,'Descriptions and captions');
  await form.getByLabel('Visual description',{exact:true}).fill('A speaker at a desk.');
  await form.getByLabel('Transcript',{exact:true}).fill('Welcome to the demonstration.');
  const help=page.locator('[data-video-help]').filter({has:page.locator('summary[aria-label="Help: Transcript"]')});
  await help.locator('summary').press('Enter');
  await expect(help.locator('.video-help__body')).toBeVisible();
  if(noScript) await help.locator('summary').press('Enter');
  else {
    await help.locator('summary').press('Escape');
    await expect(help).not.toHaveAttribute('open','');
    await expect(help.locator('summary')).toBeFocused();
    await help.locator('summary').press('Enter');
    await form.getByLabel('Transcript',{exact:true}).click();
    await expect(help).not.toHaveAttribute('open','');
    if(info.project.name.includes('mobile')) {
      await help.locator('summary').tap();
      await expect(help.locator('.video-help__body')).toBeVisible();
      await form.getByLabel('Transcript',{exact:true}).tap();
      await expect(help).not.toHaveAttribute('open','');
    } else {
      await help.locator('summary').hover();
      await expect(help.locator('.video-help__body')).toBeVisible();
      await form.getByLabel('Transcript',{exact:true}).hover();
      await expect(help).not.toHaveAttribute('open','');
    }
  }
  await open(page,'Description and publishing settings');
  await form.getByLabel('Description',{exact:true}).fill('A short introduction.');
  await form.getByRole('combobox',{name:'Licence',exact:true}).selectOption('2');
  if(!noScript) await expect(page.locator('[data-video-licence-status]')).toBeVisible();
  if(!noScript) {
    const licenceHelp=page.locator('[data-video-help]').filter({has:page.locator('summary[aria-label="Help: Licence"]')});
    await licenceHelp.locator('summary').press('Enter');
    const link=licenceHelp.getByRole('link',{name:'Learn about licences'});
    await licenceHelp.locator('summary').press('Tab');
    await expect(link).toBeFocused();
    expect(await link.evaluate(el=>{const r=el.getClientRects()[0];return document.elementFromPoint(r.left+r.width/2,r.top+r.height/2)?.closest('a')===el;})).toBe(true);
    await link.press('Escape');
    await expect(licenceHelp).not.toHaveAttribute('open','');
  }
  await audit(page,noScript);
  // Closing and reopening preserves author-entered values.
  await open(page,'Descriptions and captions');
  await open(page,'Descriptions and captions');
  await expect(form.getByLabel('Visual description',{exact:true})).toHaveValue('A speaker at a desk.');
  await page.getByRole('link',{name:'Cancel',exact:true}).press('Enter');
  await expect(page).toHaveURL(/\/videos$/);
  await page.goto('/videos/live/new');
  let live=page.locator('form[action="/web/videos/live"]');
  await expect(live.getByRole('combobox',{name:'Visibility',exact:true})).toBeVisible();
  await live.getByLabel('Video title',{exact:true}).fill('UX validation only');
  await open(page,'Description and content settings');
  await live.getByLabel('Description',{exact:true}).fill('Keep this description through errors.');
  await live.getByRole('combobox',{name:'Licence',exact:true}).selectOption('2');
  await open(page,/Playback settings/);
  await expect(live.locator('[name="dvr_minutes"]')).toHaveValue('10');
  await expect(live.locator('[name="latency"]')).toHaveValue('balanced');
  await live.locator('[name="latency"]').selectOption('low');
  await open(page,'Schedule and reuse');
  await live.locator('[name="scheduled_at"]').fill('2020-01-01T12:00');
  await live.getByRole('button',{name:'Continue to camera setup',exact:true}).press('Enter');
  // Validation happens before creation, so this journey leaves no broadcast.
  await expect(page.getByRole('alert')).toBeVisible();
  live=page.locator('form[action="/web/videos/live"]');
  await expect(live.getByLabel('Video title',{exact:true})).toHaveValue('UX validation only');
  await expect(live.getByLabel('Description',{exact:true})).toHaveValue('Keep this description through errors.');
  await expect(live.locator('[name="latency"]')).toHaveValue('low');
  await expect(live.locator('[name="scheduled_at"]')).toHaveValue('2020-01-01T12:00');
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('live-error-preserves-options.png'),fullPage:true});
  await page.getByRole('link',{name:'Cancel',exact:true}).press('Enter');
  await expect(page).toHaveURL(/\/videos$/);
  await page.goto('/videos/channels');
  await expect(page.locator('.video-workspace__row').first().locator('.video-workspace__meta')).toContainText(/Owner|Publisher|Editor|invitation/i);
  await expect(page.locator('.video-channel__view').first()).toBeVisible();
  await audit(page,noScript);
  await page.goto('/videos?view=live');
  const broadcastLink=page.locator('.video-workspace__title').first();
  if(await broadcastLink.count()) {
    await broadcastLink.press('Enter');
    if(!noScript) {
      await expect(page.getByRole('button',{name:'Use camera and microphone',exact:true})).toBeVisible();
      expect(await page.locator('[data-live-camera]').evaluate(e=>e.getBoundingClientRect().top))
        .toBeLessThan(await page.locator('summary').filter({hasText:'Schedule and reuse'}).evaluate(e=>e.getBoundingClientRect().top));
    }
    await open(page,'External encoder and stream key');
    await expect(page.getByRole('button',{name:'Rotate stream key',exact:true})).toBeVisible();
    await expect(page.locator('#live-rotate-warning')).toBeVisible();
    // Inspect the controls without rotating keys or requesting camera access.
    await audit(page,noScript);
  }
}
test('Authoring hierarchy, help dismissal and validation preserve choices',async({page},info)=>journey(page,false,info));
test('Authoring disclosures and error recovery work without scripts @no-js',async({page},info)=>journey(page,true,info));
