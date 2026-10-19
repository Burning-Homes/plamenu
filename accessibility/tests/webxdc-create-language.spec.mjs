import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {fileURLToPath} from 'node:url';
import {signIn,chooseInterfaceLanguage} from '../session.mjs';

const bundle=fileURLToPath(new URL('../fixtures/webxdc/a11y.xdc',import.meta.url));
async function language(page,code,scripted) {
  if(scripted) return chooseInterfaceLanguage(page,code);
  await page.goto('/settings/languages');
  const form=page.locator('form[action="/web/settings/languages"]');
  await form.locator('select[name=locale]').selectOption(code);
  await form.locator('button[type=submit]').press('Enter');
}
async function journey(page,info) {
  test.setTimeout(120000);
  const scripted=info.project.use.javaScriptEnabled!==false;
  if(!scripted) await page.setViewportSize({width:320,height:800});
  await signIn(page);
  await page.goto('/webxdc/library');
  const upload=page.locator('form[action="/web/webxdc/library"]');
  await upload.locator('[name=bundle]').setInputFiles(bundle);
  await upload.locator('[name=summary]').fill('Local fixture application');
  await upload.locator('button[type=submit]').press('Enter');
  await expect(page.getByRole('status')).toContainText('App saved');
  await language(page,'ru',scripted);
  try {
    await page.goto('/webxdc/new');
    const form=page.locator('form[data-webxdc-create]');
    await expect(page.locator('html')).toHaveAttribute('lang','ru');
    await form.locator('.webxdc-advanced > summary').press('Enter');
    // Normalize scroll so full-page captures do not include offscreen fixed controls.
    await page.evaluate(()=>window.scrollTo(0,0));
    await page.screenshot({path:info.outputPath('webxdc-create-russian.png'),fullPage:true});
    await expect.soft(page.locator('h1')).toHaveText('Создать сеанс приложения',{timeout:1000});
    await expect.soft(form.locator('button[type=submit]')).toHaveText('Создать сеанс',{timeout:1000});
    await expect.soft(form.locator('[data-webxdc-app-browser-label]')).toHaveText('Выбрать из библиотеки приложений',{timeout:1000});
    await form.locator('[data-webxdc-app-browser] > summary').press('Enter');
    if(scripted) {
      await form.locator('[data-webxdc-create-search]').fill('nothing-matches-this');
      await expect.soft(form.locator('[data-webxdc-create-no-results]')).toHaveText('По этому запросу нет доступных приложений.',{timeout:1000});
      await form.locator('[data-webxdc-create-search]').fill('');
    }
    await form.locator('[data-webxdc-app-choice] input[type=radio]').first().press('Space');
    if(scripted) await expect.soft(form.locator('[data-webxdc-app-browser-meta]')).toHaveText('Выбрано из библиотеки приложений',{timeout:1000});
    await form.locator('[data-webxdc-upload-choice] input[type=radio]').press('Space');
    if(scripted) await expect.soft(form.locator('[data-webxdc-app-browser-label]')).toHaveText('Выбрать из библиотеки приложений',{timeout:1000});
    await form.locator('[name=bundle]').setInputFiles({name:'broken.xdc',mimeType:'application/webxdc+zip',buffer:Buffer.from('Not a package.')});
    await form.locator('[name=name]').fill('Локальный тестовый сеанс');
    await form.locator('[name=summary]').fill('Совместная работа в тестовой среде.');
    await form.locator('[name=membership_policy]').selectOption('approval');
    await form.locator('button[type=submit]').press('Enter');
    const error=page.locator('.webxdc-create [role=alert]');
    await expect(error).toBeFocused();
    await expect.soft(error).toHaveAttribute('lang','en',{timeout:1000});
    await expect(form.locator('[name=name]')).toHaveValue('Локальный тестовый сеанс');
    // Normalize scroll so full-page captures do not include offscreen fixed controls.
    await page.evaluate(()=>window.scrollTo(0,0));
    await page.screenshot({path:info.outputPath('webxdc-create-russian-error.png'),fullPage:true});
    expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
    if(scripted) {
      const scan=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
      expect.soft(scan.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
    }
    await form.locator('[data-webxdc-app-browser] > summary').press('Enter');
    await form.locator('[data-webxdc-app-choice] input[type=radio]').first().press('Space');
    await form.locator('button[type=submit]').press('Enter');
    await expect(page.locator('.webxdc-page h1')).toHaveText('Локальный тестовый сеанс');
  } finally {await language(page,'en',scripted);}
}
test('Russian Webxdc creation localizes upload and saved-app choices',async({page},info)=>journey(page,info));
test('@no-js Russian Webxdc creation localizes native choices',async({page},info)=>journey(page,info));
