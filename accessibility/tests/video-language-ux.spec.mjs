import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {signIn,chooseVideoLanguage} from '../session.mjs';

async function bounds(page) {
  const result=await page.evaluate(()=>{
    const width=document.documentElement.clientWidth;
    const outside=[...document.querySelectorAll('main input,main select,main textarea,main button,main summary,main video')]
      .filter(e=>e.checkVisibility())
      .map(e=>({tag:e.tagName,left:e.getBoundingClientRect().left,right:e.getBoundingClientRect().right}))
      .filter(e=>e.left<0||e.right>width);
    return {width,outside};
  });
  expect(result.outside,JSON.stringify(result)).toEqual([]);
}
async function languageJourney(page,info) {
  await signIn(page);
  const noScript=info.project.use.javaScriptEnabled===false;
  for (const width of [320,375]) {
    await page.setViewportSize({width,height:800});
    for (const route of ['/videos/upload','/videos/live/new','/videos/import']) {
      await page.goto(route);
      const form=page.locator('main form').filter({has:page.locator('select[name="language"]')}).first();
      await chooseVideoLanguage(form,'en');
      await expect(form.locator('select[name="language"] option:checked')).toHaveText('English');
      if(!noScript) await expect(form.locator('[data-combo-label]')).toHaveText('English');
      await bounds(page);
    }
    const form=page.locator('form[action="/web/videos/import"]');
    await chooseVideoLanguage(form,'fr');
    await form.getByLabel('Public source URL',{exact:true}).fill('http://127.0.0.1/private.mp4');
    await form.getByLabel('Video title',{exact:true}).fill('Preserve readable language');
    await form.getByRole('checkbox',{name:/I have permission/}).check();
    const refused=page.waitForResponse(r=>r.request().method()==='POST'&&new URL(r.url()).pathname==='/web/videos/import');
    await form.getByRole('button',{name:'Import into a private draft'}).press('Enter');
    expect((await refused).status()).toBe(422);
    await expect(form.locator('select[name="language"]')).toHaveValue('fr');
    await expect(form.locator('select[name="language"] option:checked')).toContainText('French');
    await bounds(page);
    if(!noScript) {
      const help=page.locator('summary[aria-label="Help: Licence"]');
      await help.scrollIntoViewIfNeeded();
      // Put the help control near the usable bottom, without changing its layout.
      await page.evaluate(()=>{
        const e=document.querySelector('summary[aria-label="Help: Licence"]');
        const bar=document.querySelector('.tabbar');
        const bottom=bar&&getComputedStyle(bar).display!=='none'?bar.getBoundingClientRect().top:innerHeight;
        window.scrollBy(0,e.getBoundingClientRect().bottom-(bottom-8));
      });
      await help.press('Enter');
      await expect(page.locator('.video-help[open] .video-help__body')).toBeVisible();
      await expect.poll(()=>page.evaluate(()=>{
        const r=document.querySelector('.video-help[open] .video-help__body').getBoundingClientRect();
        const bar=document.querySelector('.tabbar');
        const bottom=bar&&getComputedStyle(bar).display!=='none'?bar.getBoundingClientRect().top:innerHeight;
        return r.top>=0&&r.bottom<=bottom&&r.left>=0&&r.right<=document.documentElement.clientWidth;
      })).toBe(true);
      await page.screenshot({path:info.outputPath(`help-near-bottom-${width}.png`),fullPage:false});
      await help.press('Escape');
      const trigger=form.locator('[data-combo-trigger]');
      await trigger.scrollIntoViewIfNeeded();
      await page.evaluate(()=>{
        const e=document.querySelector('[data-combo-trigger]');
        const bar=document.querySelector('.tabbar');
        const bottom=bar&&getComputedStyle(bar).display!=='none'?bar.getBoundingClientRect().top:innerHeight;
        window.scrollBy(0,e.getBoundingClientRect().bottom-(bottom-8));
      });
      await trigger.press('Enter');
      await expect.poll(()=>page.evaluate(()=>{
        const r=document.querySelector('[data-combo-pop]:not([hidden])').getBoundingClientRect();
        const bar=document.querySelector('.tabbar');
        const bottom=bar&&getComputedStyle(bar).display!=='none'?bar.getBoundingClientRect().top:innerHeight;
        return r.top>=0&&r.bottom<=bottom&&r.left>=0&&r.right<=document.documentElement.clientWidth;
      })).toBe(true);
      await page.screenshot({path:info.outputPath(`language-near-bottom-${width}.png`),fullPage:false});
      await form.locator('[data-combo-search]').press('Escape');
      const a=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
      expect(a.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
    }
  }
}
test('Readable video languages retain values and popups fit near the bottom',async({page},info)=>languageJourney(page,info));
test('Readable video languages retain values through native no-JavaScript forms @no-js',async({page},info)=>languageJourney(page,info));

test('Translated video language labels fit narrow screens',async({page},info)=>{
  await signIn(page);
  await page.goto('/settings/languages');
  const form=page.locator('form[action="/web/settings/languages"]');
  const csrf=await form.locator('input[name="csrf"]').inputValue();
  const changed=await page.request.post('/web/settings/languages',{form:{csrf,locale:'ru'},maxRedirects:0});
  expect(changed.status()).toBe(303);
  try {
    for(const width of [320,375]) {
      await page.setViewportSize({width,height:800});
      await page.goto('/videos/import');
      await expect(page.locator('html')).toHaveAttribute('lang','ru');
      const f=page.locator('form[action="/web/videos/import"]');
      await chooseVideoLanguage(f,'en');
      await expect(page.locator('main')).toContainText('Язык видео');
      await bounds(page);
      await page.locator('h1').click();
      await page.screenshot({path:info.outputPath(`russian-import-${width}.png`),fullPage:true});
      await page.goto('/videos/live/new');
      const live=page.locator('form[action="/web/videos/live"]');
      await chooseVideoLanguage(live,'fr');
      await bounds(page);
      await page.locator('h1').click();
      await page.screenshot({path:info.outputPath(`russian-live-${width}.png`),fullPage:true});
    }
  } finally {
    await page.goto('/settings/languages');
    const restoredCsrf=await page.locator('form[action="/web/settings/languages"] input[name="csrf"]').inputValue();
    const restored=await page.request.post('/web/settings/languages',{form:{csrf:restoredCsrf,locale:'en'},maxRedirects:0});
    expect(restored.status()).toBe(303);
  }
});
