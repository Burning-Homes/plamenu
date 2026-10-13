import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {signIn,openDetailsFor} from '../session.mjs';
import {randomUUID} from 'node:crypto';

async function audit(page,noScript){
  const geometry=await page.evaluate(()=>({width:innerWidth,scroll:document.documentElement.scrollWidth,overflow:[...document.querySelectorAll('*')].filter(e=>e.getBoundingClientRect().width>0&&e.getBoundingClientRect().right>innerWidth+1).map(e=>({tag:e.tagName,class:e.className,right:e.getBoundingClientRect().right,text:e.textContent.slice(0,60)}))}));
  expect(geometry.scroll,JSON.stringify(geometry)).toBeLessThanOrEqual(geometry.width);
  if(!noScript){
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
    expect(result.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
}
async function journey(page,info,noScript){
  test.skip(!process.env.A11Y_PODCAST_SOURCE_URL,'Enable the disposable RSS/enclosure producer with A11Y_PODCAST_FIXTURE=1');
  const title=`Podcast ${randomUUID().slice(0,8)}`;
  const feedUrl=process.env.A11Y_PODCAST_SOURCE_URL+'?'+new URLSearchParams({title});
  const website=new URL(feedUrl);website.pathname='/show';
  await signIn(page);
  // The player journey can leave history disabled. Establish this journey's
  // explicit listening preference before testing elapsed playback reports.
  await page.goto('/music/history');
  await page.getByText('Listening preferences',{exact:true}).press('Enter');
  await page.getByLabel('Keep my listening history',{exact:true}).check();
  await page.getByLabel('Minimum playback seconds',{exact:true}).fill('2');
  await page.getByLabel('Minimum playback percentage',{exact:true}).fill('100');
  await Promise.all([page.waitForNavigation({waitUntil:'domcontentloaded'}),page.getByRole('button',{name:'Save listening preferences',exact:true}).press('Enter')]);
  await page.goto('/music/collections');
  const create=page.locator('form[action="/web/music/collections/create"]');
  await openDetailsFor(create);
  await create.getByLabel('Name',{exact:true}).fill(title+' playlist');
  await create.locator('select[name="kind"]').selectOption('playlist');
  await create.getByRole('button',{name:'Create collection',exact:true}).press('Enter');
  await page.waitForURL(url=>/^\/music\/collections\/\d+$/.test(url.pathname));
  const collection=page.url();
  await page.goto('/music');
  await expect(page.locator('a.nav-link[href="/music/podcasts"],a.nav-link[href="/music/radios"]')).toHaveCount(0);
  await page.getByRole('link',{name:'Podcasts',exact:true}).press('Enter');
  await expect(page).toHaveURL(url=>url.pathname==='/music/podcasts');
  const subscribe=page.locator('form[action="/web/music/podcasts/subscribe"]');
  await openDetailsFor(subscribe);
  await subscribe.getByLabel('Podcast website or feed link',{exact:true}).fill(website.href);
  await subscribe.getByRole('button',{name:'Follow a podcast',exact:true}).press('Enter');
  await page.waitForURL(url=>/^\/music\/podcasts\/\d+$/.test(url.pathname));
  const show=page.url();
  await expect(page.getByRole('heading',{name:title,exact:true})).toBeVisible();
  await expect.poll(()=>page.locator('img.music-artwork').first().evaluate(i=>i.complete&&i.naturalWidth>0)).toBe(true);
  await expect(page.locator('input[name="episode"]')).toHaveCount(2);
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('music-podcast-show.png'),fullPage:true});
  await openDetailsFor(page.locator('input[name="episode"]').first());
  await page.locator('input[name="episode"]').first().uncheck();
  await openDetailsFor(page.getByRole('button',{name:'Mark selected episodes as played',exact:true,includeHidden:true}));
  await Promise.all([page.waitForNavigation({waitUntil:'domcontentloaded'}),page.getByRole('button',{name:'Mark selected episodes as played',exact:true}).press('Enter')]);
  await expect(page.locator('ol > li').filter({hasText:'Played'})).toHaveCount(1);
  await openDetailsFor(page.locator('input[name="episode"]').first());
  await page.locator('input[name="episode"]').first().uncheck();
  await openDetailsFor(page.locator('#podcast-bulk-collection'));
  await page.locator('#podcast-bulk-collection').selectOption({label:title+' playlist'});
  await page.getByRole('button',{name:'Add selected episodes to collection',exact:true}).press('Enter');
  await expect(page).toHaveURL(collection);
  await expect(page.locator('ol').getByRole('link',{name:`${title} 1`,exact:true})).toHaveCount(1);
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('music-podcast-collection.png'),fullPage:true});
  await page.goto(show);
  await page.getByRole('link',{name:`${title} 1`,exact:true}).press('Enter');
  const episode=page.url();
  const audio=page.locator('audio[data-podcast-episode]');
  await expect(audio).toHaveAttribute('src',/\/music\/podcasts\/episodes\/\d+\/audio$/);
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('music-podcast-episode.png'),fullPage:true});
  // Real decoding and elapsed reports use the native controls, never fabricated events.
  if(!noScript){
    await page.getByRole('button',{name:'Mark as unplayed',exact:true}).press('Enter');
    await expect(page.locator('.podcast-played-state')).toHaveText('Unplayed');
    // Native listening must still be counted if the enhancement script arrives
    // after Play. Delaying the real script reproduces the early-play race.
    let releaseScript;
    const scriptGate=new Promise(resolve=>{releaseScript=resolve;});
    const scriptRoute=async route=>{await scriptGate;await route.continue();};
    await page.route('**/assets/app.js*',scriptRoute);
    try{
      await page.goto(episode,{waitUntil:'commit'});
      await expect(audio).toBeVisible();
      if(info.project.name.startsWith('webkit')){const box=await audio.boundingBox();await audio.click({position:{x:20,y:box.height/2}});}else await audio.press('Space');
      await expect.poll(()=>audio.evaluate(a=>a.readyState),{timeout:15000}).toBeGreaterThan(0);
      await expect.poll(()=>audio.evaluate(a=>a.currentTime),{timeout:15000}).toBeGreaterThan(1);
      const started=page.waitForResponse(response=>response.request().method()==='POST'&&new URL(response.url()).pathname==='/web/music/listening/start');
      releaseScript();
      expect((await (await started).json()).enabled).toBe(true);
      await expect(page.locator('.podcast-played-state')).toHaveText('Played',{timeout:15000});
    }finally{releaseScript();await page.unroute('**/assets/app.js*',scriptRoute);}
  }
  await page.getByRole('button',{name:'Mark as unplayed',exact:true}).press('Enter');
  await expect(page.locator('.podcast-played-state')).toHaveText('Unplayed');
  await page.goto(show);
  const popup=page.waitForEvent('popup');
  await openDetailsFor(page.getByRole('link',{name:'Queue these episodes',exact:true,includeHidden:true}));
  await page.getByRole('link',{name:'Queue these episodes',exact:true}).press('Enter');
  const player=await popup;
  await player.waitForLoadState('domcontentloaded');
  await player.getByLabel('Replace current queue',{exact:true}).check();
  await player.getByRole('button',{name:'Add collection to queue',exact:true}).press('Enter');
  await expect(player.locator('.music-player-entries > li')).toHaveCount(2);
  const selectTrack=player.locator('.music-player-entries > li').first().getByRole('button',{name:'Play this track',exact:true,includeHidden:true});
  await openDetailsFor(selectTrack);
  await selectTrack.press('Enter');
  await expect(player.locator('section.music-player')).not.toHaveAttribute('data-current','');
  if(!noScript) await expect.poll(()=>player.locator('audio.music-player-audio').evaluate(a=>a.currentTime),{timeout:15000}).toBeGreaterThan(1);
  await audit(player,noScript);
  await player.screenshot({path:info.outputPath('music-podcast-queue.png'),fullPage:true});
  await player.close();
  await page.goto('/music/podcasts');
  const upload=page.locator('form[action="/web/music/podcasts/import-file"]');
  await openDetailsFor(upload);
  await upload.locator('input[type="file"]').setInputFiles({name:'podcasts.opml',mimeType:'text/x-opml',buffer:Buffer.from(`<opml><body><outline xmlUrl="${feedUrl}"/></body></opml>`)});
  await upload.getByRole('button',{name:'Import subscriptions',exact:true}).press('Enter');
  await expect(page.getByText(/Already followed: 1/)).toBeVisible();
  await page.goto(show);
  await page.getByRole('button',{name:'Unfollow podcast',exact:true}).press('Enter');
  await expect(page.getByRole('link',{name:title,exact:true})).toHaveCount(0);
  expect(episode).toContain('/music/podcasts/episodes/');
}
test('podcasts browse and play inside one Music workspace',async({page},info)=>journey(page,info,false));
test('podcast management works without JavaScript @no-js',async({page},info)=>journey(page,info,true));
