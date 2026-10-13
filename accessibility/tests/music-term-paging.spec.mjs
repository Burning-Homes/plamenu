import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {randomUUID} from 'node:crypto';
import {signIn} from '../session.mjs';
async function journey(page,info,noScript){
  test.setTimeout(180000);
  await signIn(page);await page.goto('/music/collections');
  const csrf=await page.locator('form[action="/web/music/collections/create"] input[name="csrf"]').inputValue();
  const tag='pages_'+randomUUID().replaceAll('-','');
  const titles=[];
  // Ordinary native CSRF endpoints seed the longer catalogue. Navigation below
  // exercises the human-facing link in each browser mode.
  for(let index=0;index<65;index++){
    const title=`Paged collection ${index} ${tag}`;titles.push(title);
    const created=await page.request.post('/web/music/collections/create',{form:{csrf,name:title,kind:'playlist',visibility:'public',description:''},maxRedirects:0});
    expect(created.status()).toBe(303);
    const path=created.headers().location;
    const id=path.split('/').at(-1);
    const tagged=await page.request.post(`/web/music/collections/${id}/tags`,{form:{csrf,revision:'1',tags:tag},maxRedirects:0});
    expect(tagged.status()).toBe(303);
  }
  await page.setViewportSize({width:320,height:800});
  await page.goto(`/music/tags/${tag}`);
  const links=page.locator('section.music-library > ul > li > a');
  await expect(links).toHaveCount(60);
  const first=await links.allTextContents();
  await expect(page.locator('a[rel="next"]')).toBeVisible();
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
  if(!noScript){const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();expect(result.violations).toEqual([]);}
  await page.screenshot({path:info.outputPath('music-tag-first-page.png'),fullPage:true});
  await page.locator('a[rel="next"]').press('Enter');
  await expect(page).toHaveURL(url=>url.searchParams.has('before'));
  await expect(links).toHaveCount(5);
  await expect(page.locator('a[rel="next"]')).toHaveCount(0);
  const second=await links.allTextContents();
  expect(new Set([...first,...second]).size).toBe(65);
  expect([...first,...second].sort()).toEqual(titles.sort());
  await expect(page.getByRole('link',{name:'RSS feed',exact:true})).toBeVisible();
  await expect(page.getByRole('link',{name:'Atom feed',exact:true})).toBeVisible();
  await expect(page.locator('aside.sidebar a.nav-link[href="/music"]')).toHaveCount(1);
  await expect(page.locator('a.nav-link[href="/music/podcasts"],a.nav-link[href="/music/radios"]')).toHaveCount(0);
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
  await page.screenshot({path:info.outputPath('music-tag-next-page.png'),fullPage:true});
}
test('Music tag browsing continues past its first bounded page',async({page},info)=>journey(page,info,false));
test('Music tag pagination works without JavaScript @no-js',async({page},info)=>journey(page,info,true));
