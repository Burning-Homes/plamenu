import {freshMusicAudio} from '../session.mjs';
import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {signIn,openDetailsFor} from '../session.mjs';
import {randomUUID} from 'node:crypto';

async function audit(page,noScript){
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
  if(!noScript){
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
    expect(result.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
}
async function feeds(page){
  for(const [name,type] of [['RSS feed','application/rss+xml'],['Atom feed','application/atom+xml']]){
    const link=page.getByRole('link',{name,exact:true,includeHidden:true});
    await openDetailsFor(link);
    const response=await page.request.get(await link.getAttribute('href'));
    expect(response.status()).toBe(200);
    expect(response.headers()['content-type']).toContain(type);
    const xml=await response.text();
    const parsed=await page.evaluate(xml=>{
      const root=new DOMParser().parseFromString(xml,'application/xml');
      return {error:root.querySelector('parsererror')?.textContent??null,count:root.querySelectorAll('item,entry').length};
    },xml);
    expect(parsed.error).toBe(null);
    expect(parsed.count).toBeGreaterThan(0);
  }
}
async function journey(page,browser,info,noScript){
  test.setTimeout(180000);
  const title='Feed music '+randomUUID().slice(0,8);
  const tag='qa_'+randomUUID().replaceAll('-','');
  await signIn(page);
  await page.goto('/music/assets?mine=true');
  const upload=page.locator('form[action="/web/music/assets/import"]');
  await openDetailsFor(upload);
  await upload.getByLabel('Audio file',{exact:true}).setInputFiles(await freshMusicAudio());
  await upload.getByRole('button',{name:'Upload audio',exact:true}).press('Enter');
  await page.waitForURL(url=>/^\/music\/assets\/\d+$/.test(url.pathname));
  const track=page.url();
  const id=new URL(track).pathname.split('/').at(-1);
  const edit=page.locator(`form[action="/web/music/assets/${id}/edit"]`);
  await openDetailsFor(edit);
  await edit.getByLabel('Title',{exact:true}).fill(title);
  await edit.getByLabel('Artist',{exact:true}).fill(title+' artist');
  await edit.getByLabel('Genre',{exact:true}).fill('Ambient');
  await edit.getByLabel('Who can listen',{exact:true}).selectOption('public');
  await Promise.all([page.waitForNavigation({waitUntil:'domcontentloaded'}),edit.getByRole('button',{name:'Save track',exact:true}).press('Enter')]);
  const details=page.locator(`form[action="/web/music/assets/${id}/details"]`);
  await openDetailsFor(details);
  await details.getByLabel('Tags',{exact:true}).fill(tag);
  await Promise.all([page.waitForNavigation({waitUntil:'domcontentloaded'}),details.getByRole('button',{name:'Save track details',exact:true}).press('Enter')]);
  await page.getByRole('link',{name:tag,exact:true}).press('Enter');
  await expect(page.getByRole('link',{name:title+' artist — '+title,exact:true})).toBeVisible();
  await feeds(page);
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('music-tag-browse.png'),fullPage:true});
  await page.goto(track);
  await page.getByRole('link',{name:'Ambient',exact:true}).press('Enter');
  await feeds(page);
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('music-genre-browse.png'),fullPage:true});
  await page.goto(track);
  await page.getByRole('link',{name:title+' artist',exact:true}).press('Enter');
  await feeds(page);
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('music-artist-release-feeds.png'),fullPage:true});
  await page.goto('/music/collections');
  const create=page.locator('form[action="/web/music/collections/create"]');
  await openDetailsFor(create);
  await create.getByLabel('Name',{exact:true}).fill(title+' playlist');
  await create.locator('select[name="kind"]').selectOption('playlist');
  await create.locator('select[name="visibility"]').selectOption('public');
  await create.getByRole('button',{name:'Create collection',exact:true}).press('Enter');
  await page.waitForURL(url=>/^\/music\/collections\/\d+$/.test(url.pathname));
  const collection=page.url();
  const collectionId=new URL(collection).pathname.split('/').at(-1);
  const add=page.locator(`form[action="/web/music/collections/${collectionId}/add"]`);
  await openDetailsFor(add);
  await add.getByLabel('Track, Audio post or podcast episode URL',{exact:true}).fill(track);
  await add.getByRole('button',{name:'Add a track',exact:true}).press('Enter');
  const tags=page.locator(`form[action="/web/music/collections/${collectionId}/tags"]`);
  await openDetailsFor(tags);
  await tags.getByLabel('Tags',{exact:true}).fill(tag);
  await Promise.all([page.waitForNavigation({waitUntil:'domcontentloaded'}),tags.getByRole('button',{name:'Save collection tags',exact:true}).press('Enter')]);
  await feeds(page);
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('music-collection-tags-and-feeds.png'),fullPage:true});
  await page.getByRole('link',{name:tag,exact:true}).press('Enter');
  await expect(page.getByRole('link',{name:title+' playlist',exact:true})).toBeVisible();
  await page.goto(collection);
  await page.getByRole('link',{name:'Create share link',exact:true}).press('Enter');
  await page.getByRole('button',{name:'Create share link',exact:true}).press('Enter');
  const shared=await page.locator('#music-share-url').inputValue();
  const external=await browser.newContext({baseURL:process.env.A11Y_BASE_URL,ignoreHTTPSErrors:false,javaScriptEnabled:!noScript});
  try{
    const guest=await external.newPage();
    const url=new URL(shared);
    await guest.goto(url.pathname+url.search);
    await expect(guest.getByRole('heading',{name:title+' playlist',exact:true})).toBeVisible();
    await feeds(guest);
    await audit(guest,noScript);
    await guest.screenshot({path:info.outputPath('music-shared-collection-feeds.png'),fullPage:true});
  }finally{await external.close();}
  await page.goto('/music');
  await expect(page.locator('aside.sidebar a.nav-link[href="/music"]')).toHaveCount(1);
  await expect(page.locator('details.drawer a.nav-link[href="/music"]')).toHaveCount(1);
  await expect(page.locator('a.nav-link[href="/music/podcasts"],a.nav-link[href="/music/radios"]')).toHaveCount(0);
}
test('music feeds and discovery use one Music menu',async({page,browser},info)=>journey(page,browser,info,false));
test('music feeds and discovery work without JavaScript @no-js',async({page,browser},info)=>journey(page,browser,info,true));
