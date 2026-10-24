import {test,expect} from '../fixtures.mjs';
import {writeFile} from 'node:fs/promises';
import {signIn,chooseInterfaceLanguage} from '../session.mjs';

// Simulate a user stylesheet for WCAG 2.2 1.4.12; do not change other properties.
const spacing=['* {line-height:1.5 !important;letter-spacing:0.12em !important;word-spacing:0.16em !important}', 'p {margin-bottom:2em !important}'];
async function inspect(page,info,name) {
  if(name==='compose'||name==='post') {
    await page.evaluate(()=>scrollTo(0,0));
    await page.screenshot({path:info.outputPath(`text-spacing-${name}-default.png`),fullPage:true});
    const normal=await page.evaluate(()=>({width:document.documentElement.clientWidth,scroll:document.documentElement.scrollWidth}));
    await writeFile(info.outputPath(`${name}-default-geometry.json`),JSON.stringify(normal)+'\n');
    expect.soft(normal.scroll,`${name}: default layout overflow`).toBeLessThanOrEqual(normal.width+1);
  }
  // Use the loaded same-origin stylesheet's CSSOM to model a user override.
  // The production CSP correctly refuses arbitrary inline style elements.
  await page.evaluate(rules=>{
    const sheet=[...document.styleSheets].find(s=>s.href&&new URL(s.href).origin===location.origin);
    if(!sheet) throw new Error('No same-origin stylesheet available for spacing survey');
    for(const rule of rules) sheet.insertRule(rule,sheet.cssRules.length);
  },spacing);
  await page.evaluate(async()=>{
    await document.fonts.ready;
    await Promise.all(document.getAnimations().filter(a=>a.effect?.getTiming().iterations!==Infinity).map(a=>a.finished.catch(()=>{})));
    scrollTo(0,0);
  });
  const geometry=await page.evaluate(()=>({
    width:document.documentElement.clientWidth,innerWidth,scroll:document.documentElement.scrollWidth,
    candidates:[...document.querySelectorAll('main button,main summary,main label,main h1,main h2,main legend')]
      .filter(e=>e.getClientRects().length&&getComputedStyle(e).visibility!=='hidden')
      .filter(e=>e.scrollWidth>e.clientWidth+2||e.scrollHeight>e.clientHeight+2)
      .map(e=>({tag:e.tagName,class:e.className,text:e.innerText?.trim().slice(0,120),
        overflow:getComputedStyle(e).overflow,textOverflow:getComputedStyle(e).textOverflow,
        width:e.clientWidth,scrollWidth:e.scrollWidth,height:e.clientHeight,scrollHeight:e.scrollHeight})),
  }));
  const applied=await page.locator('body').evaluate(e=>{
    const s=getComputedStyle(e),font=parseFloat(s.fontSize);
    const p=document.querySelector('p'),ps=p&&getComputedStyle(p);
    return {line:parseFloat(s.lineHeight)/font,letter:parseFloat(s.letterSpacing)/font,word:parseFloat(s.wordSpacing)/font,
      paragraph:ps?parseFloat(ps.marginBottom)/parseFloat(ps.fontSize):null};
  });
  expect(applied.line).toBeCloseTo(1.5,2);
  expect(applied.letter).toBeCloseTo(.12,2);
  expect(applied.word).toBeCloseTo(.16,2);
  if(applied.paragraph!==null) expect(applied.paragraph).toBeCloseTo(2,2);
  const evidence=info.outputPath(`${name}-spacing-geometry.json`);
  await writeFile(evidence,JSON.stringify(geometry,null,2)+'\n');
  await info.attach(`${name}-spacing-geometry`,{path:evidence,contentType:'application/json'});
  await page.screenshot({path:info.outputPath(`text-spacing-${name}.png`),fullPage:true});
  expect.soft(geometry.scroll,`${name}: page overflow with user text spacing`).toBeLessThanOrEqual(geometry.width+1);
  // Candidate clipping is reviewed visually; scrollable/expandable controls can be valid.
}
async function language(page,code,scripted) {
  if(scripted) return chooseInterfaceLanguage(page,code);
  await page.goto('/settings/languages');
  const form=page.locator('form[action="/web/settings/languages"]');
  await form.locator('select[name=locale]').selectOption(code);
  await form.locator('button[type=submit]').press('Enter');
}
async function journey(page,info,russian) {
  test.setTimeout(120000);
  if(info.project.use.javaScriptEnabled===false) await page.setViewportSize({width:320,height:800});
  await page.goto('/login');
  await inspect(page,info,'login');
  await page.goto('/signup');
  await inspect(page,info,'signup');
  await signIn(page);
  const scripted=info.project.use.javaScriptEnabled!==false;
  try {
    if(russian) await language(page,'ru',scripted);
    await page.goto('/compose');
    await expect(page.locator('html')).toHaveAttribute('lang',russian?'ru':'en-US');
    await inspect(page,info,'compose');
    const text=`Text spacing reading journey ${info.project.name}.\n\nA second paragraph remains available with wider spacing.`;
    await page.locator('textarea[name=status]').fill(text);
    await page.locator('button[name=op][value=post]').press('Enter');
    await expect(page).toHaveURL(url=>/^\/@[^/]+\/\d+$/.test(url.pathname));
    await inspect(page,info,'post');
    await expect(page.locator('.status__content').filter({hasText:'A second paragraph remains available'})).toBeVisible();
    await page.goto('/settings/profile');
    await inspect(page,info,'profile');
  } finally {if(russian) await language(page,'en',scripted);}
}
for(const [name,locale,russian] of [['English','en-US',false],['Russian','ru-RU',true]]) {
  test.describe(name,()=>{
    test.use({locale});
    test('core pages and publication tolerate user text-spacing overrides',async({page},info)=>journey(page,info,russian));
    test('@no-js core pages and publication tolerate text-spacing overrides',async({page},info)=>journey(page,info,russian));
  });
}
