import {freshMusicAudio} from '../session.mjs';
import {test,expect} from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import {signIn,openDetailsFor} from '../session.mjs';
import {randomUUID} from 'node:crypto';

async function audit(page,noScript){
  const geometry=await page.evaluate(()=>({width:innerWidth,documentWidth:document.documentElement.scrollWidth,controls:[...document.querySelectorAll('.music-player-page .settings-form, #music-player-destination')].map(e=>{const c=getComputedStyle(e);return {tag:e.tagName,id:e.id,grid:c.gridTemplateColumns,width:c.width,min:c.minWidth,max:c.maxWidth,overflow:c.overflow,textOverflow:c.textOverflow,appearance:c.appearance,boxSizing:c.boxSizing,children:[...e.children].map(v=>({tag:v.tagName,type:v.type,width:v.getBoundingClientRect().width,scroll:v.scrollWidth,client:v.clientWidth,text:v.textContent.slice(0,60)}))}}),overflow:[...document.querySelectorAll('*')].filter(e=>e.getBoundingClientRect().width>0&&(e.getBoundingClientRect().right>innerWidth||e.scrollWidth>e.clientWidth+1)).map(e=>({tag:e.tagName,class:e.className,width:e.getBoundingClientRect().width,right:e.getBoundingClientRect().right,scrollWidth:e.scrollWidth,clientWidth:e.clientWidth,text:e.textContent.slice(0,80)}))}));
  if(geometry.documentWidth>geometry.width){await page.screenshot({path:test.info().outputPath('music-player-overflow.png'),fullPage:true});}
  expect(geometry.documentWidth,JSON.stringify(geometry)).toBeLessThanOrEqual(geometry.width);
  if(!noScript){
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
    expect(result.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
}
async function submitForm(player,form){
  const revision=await player.locator('section.music-player').getAttribute('data-revision');
  const finished=player.waitForResponse(r=>r.request().method()==='POST'&&new URL(r.url()).pathname==='/web/music/player');
  await form.press('Enter');
  const response=await finished;
  expect([200,303]).toContain(response.status());
  await expect(player.locator('section.music-player')).not.toHaveAttribute('data-revision',revision);
  await expect(player.locator('.music-player-page')).not.toHaveAttribute('aria-busy','true');
}
async function command(player,name){
  const button=player.getByRole('button',{name,exact:true,includeHidden:true}).first();
  await openDetailsFor(button);
  await submitForm(player,button);
}
async function nativePlay(audio,info){
  if(await audio.evaluate(a=>a.paused)){
  if(info.project.name.startsWith('webkit')){
    const box=await audio.boundingBox();
    await audio.click({position:{x:20,y:box.height/2}});
  }else await audio.press('Space');
  }
  await expect.poll(()=>audio.evaluate(a=>a.currentTime),{timeout:15000}).toBeGreaterThan(0);
  await expect.poll(()=>audio.evaluate(a=>a.paused)).toBe(false);
  expect(await audio.evaluate(a=>a.error?.code||null)).toBeNull();
}
async function favouriteNotification(page,info,noScript,assetUrl,assetId,title){
  const username='music_viewer_'+info.project.name.replaceAll('-','_');
  await openDetailsFor(page.getByText('Share with a member',{exact:true}));
  const sharing=page.locator(`form[action="/web/music/assets/${assetId}/grant"]`);
  await sharing.getByLabel('Local username',{exact:true}).fill(username);
  await sharing.getByRole('button',{name:'Grant access',exact:true}).press('Enter');
  await expect(page.getByRole('heading',{name:title,exact:true})).toBeVisible();
  const origin=new URL(assetUrl).origin;
  const context=await page.context().browser().newContext({...info.project.use,baseURL:origin,viewport:info.project.use.viewport,
    javaScriptEnabled:!noScript,serviceWorkers:'block',extraHTTPHeaders:{...info.project.use.extraHTTPHeaders,Origin:process.env.A11Y_CANONICAL_ORIGIN||origin}});
  const viewer=await context.newPage();
  try{
    await viewer.goto('/login');
    await viewer.locator('input[name="identifier"]').fill(username);
    await viewer.locator('input[name="password"]').fill(process.env.A11Y_PASSWORD||'plamenu-development-only');
    await Promise.all([viewer.waitForURL(u=>u.pathname==='/'),viewer.locator('button[type="submit"]').press('Enter')]);
    await viewer.goto(assetUrl);
    await viewer.getByRole('button',{name:'Like',exact:true}).press('Enter');
    await expect(viewer.getByRole('button',{name:'Unlike',exact:true})).toBeVisible();
    await page.goto('/notifications');
    const notification=page.locator('article.notification[data-kind="music.favourite"]').filter({has:page.getByRole('link',{name:title,exact:true})});
    await expect(notification).toHaveCount(1);
    await expect(notification).toContainText('saved your track as a music favourite');
    await audit(page,noScript);
    await page.screenshot({path:info.outputPath('music-favourite-notification.png'),fullPage:true});
    await viewer.getByRole('button',{name:'Unlike',exact:true}).press('Enter');
    await expect(viewer.getByRole('button',{name:'Like',exact:true})).toBeVisible();
    await page.reload();
    await expect(notification).toHaveCount(0);
  }finally{await context.close();}
  await page.goto(assetUrl);
  await openDetailsFor(page.getByText('Share with a member',{exact:true}));
  const revoke=page.locator(`form[action="/web/music/assets/${assetId}/grant"]`);
  await revoke.getByLabel('Local username',{exact:true}).fill(username);
  await revoke.getByRole('button',{name:'Revoke access',exact:true}).press('Enter');
  await expect(page.getByRole('heading',{name:title,exact:true})).toBeVisible();
}
async function statistics(page,info,noScript,title){
  await page.goto('/music/stats');
  await expect(page.getByRole('heading',{name:'Listening statistics',exact:true})).toBeVisible();
  for(const heading of ['Most played tracks','Most played artists','Most played albums','Most played genres','Plays over time','Genres over time','Release dates','Listening by local hour']){
    await expect(page.getByRole('heading',{name:heading,exact:true})).toBeVisible();
  }
  const total=Number(await page.locator('.music-stats-total').getAttribute('data-count'));
  if(noScript){expect(total).toBe(0);}else{
    expect(total).toBeGreaterThan(0);
    for(const [heading,value] of [['Most played tracks',title],['Most played artists','Queue performer'],['Most played albums','Queue album'],['Most played genres','ambient'],['Release dates','2020s']]){
      const card=page.locator('section.music-stats-card').filter({has:page.getByRole('heading',{name:heading,exact:true})});
      await expect(card.locator('tbody')).toContainText(value);
      await expect(card.locator('tbody tr').first().locator('td')).toContainText(String(total));
    }
    const image=page.locator('img.music-stats-artwork').first();
    await expect.poll(()=>image.evaluate(i=>i.complete&&i.naturalWidth>0)).toBe(true);
  }
  const isoDate=offset=>new Date(Date.now()+offset*86400000).toISOString().slice(0,10);
  await page.getByLabel('Start date',{exact:true}).fill(isoDate(-2));
  await page.getByLabel('End date',{exact:true}).fill(isoDate(2));
  await page.getByLabel('Time zone',{exact:true}).selectOption('Asia/Tbilisi');
  await page.getByLabel('Group by',{exact:true}).selectOption('month');
  await page.getByLabel('First day of the week',{exact:true}).selectOption('6');
  await page.getByLabel('Group release dates by',{exact:true}).selectOption('year');
  await page.getByLabel('Ranked items per chart',{exact:true}).fill('5');
  await Promise.all([page.waitForNavigation({waitUntil:'domcontentloaded'}),page.getByRole('button',{name:'Show statistics',exact:true}).press('Enter')]);
  await expect(page.getByLabel('Time zone',{exact:true})).toHaveValue('Asia/Tbilisi');
  await expect(page.getByLabel('Group by',{exact:true})).toHaveValue('month');
  await expect(page.getByLabel('First day of the week',{exact:true})).toHaveValue('6');
  await expect(page.getByLabel('Group release dates by',{exact:true})).toHaveValue('year');
  await expect(page.locator('.music-stats-total')).toHaveAttribute('data-count',String(total));
  if(!noScript){const releases=page.locator('section.music-stats-card').filter({has:page.getByRole('heading',{name:'Release dates',exact:true})});await expect(releases.locator('tbody')).toContainText('2021');}
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('music-listening-statistics.png'),fullPage:true});
  await page.goto('/music/history');
}
async function journey(page,info,noScript){
  test.setTimeout(180000);
  await signIn(page);
  await page.goto('/music/assets?mine=true');
  await page.getByRole('link',{name:'Upload audio',exact:true}).press('Enter');
  await page.getByLabel('Audio file',{exact:true}).setInputFiles(await freshMusicAudio());
  await page.getByRole('button',{name:'Save privately',exact:true}).press('Enter');
  await page.waitForURL(u=>/^\/music\/assets\/\d+$/.test(u.pathname));
  const assetUrl=page.url();
  const assetId=new URL(assetUrl).pathname.split('/').at(-1);
  const title='Queue recording '+randomUUID();
  await openDetailsFor(page.getByText('Edit track',{exact:true}));
  const edit=page.locator(`form[action="/web/music/assets/${assetId}/edit"]`);
  await edit.getByLabel('Title',{exact:true}).fill(title);
  await edit.getByLabel('Artist',{exact:true}).fill('Queue performer');
  await edit.getByLabel('Album',{exact:true}).fill('Queue album');
  await edit.getByLabel('Genre',{exact:true}).fill('Ambient');
  await edit.getByLabel('Release year',{exact:true}).fill('2021');

  await edit.getByLabel('Who can listen',{exact:true}).selectOption('private');
  await edit.getByRole('button',{name:'Save track',exact:true}).press('Enter');
  await expect(page.getByRole('heading',{name:title,exact:true})).toBeVisible();
  await openDetailsFor(page.getByText('Change artwork',{exact:true}));
  await page.getByLabel('Image file',{exact:true}).setInputFiles(process.env.A11Y_POSTER_FIXTURE);
  await page.getByRole('button',{name:'Upload image',exact:true}).press('Enter');
  await expect(page.locator('img.music-artwork')).toHaveCount(1);
  await page.getByRole('button',{name:'Like',exact:true}).press('Enter');
  await expect(page.getByRole('button',{name:'Unlike',exact:true})).toBeVisible();
  if(info.project.name.endsWith('mobile')){await page.setViewportSize({width:320,height:800});}
  await page.goto('/music/favourites');
  await expect(page.locator('section.music-favourites ul')).toContainText(title);
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('music-favourites.png'),fullPage:true});
  await page.getByRole('link',{name:title,exact:true}).press('Enter');
  await page.waitForURL(assetUrl);
  await favouriteNotification(page,info,noScript,assetUrl,assetId,title);

  const opened=page.waitForEvent('popup');
  await page.getByText('Queue options',{exact:true}).press('Enter');
  await page.getByRole('link',{name:'Open in music player',exact:true}).press('Enter');
  const player=await opened;
  try{
    await expect(player.getByRole('heading',{name:'Music player',exact:true})).toBeVisible();
    const previewUrl=player.url();
    if(await player.locator('.music-player-entries > li').count()>0){
      await command(player,'Clear queue');
      // HTML commands return to the queue; reopen the chosen-song preview.
      await player.goto(previewUrl);
    }
    await command(player,'Add to queue');
    const entries=player.locator('.music-player-entries > li');
    await expect(entries).toHaveCount(1);
    await expect(entries.first()).toContainText(title);
    await command(player,'Play this track');
    await expect(player.locator('.music-player-now')).toContainText(title);
    await expect.poll(()=>player.locator('img.music-player-artwork').evaluate(img=>img.complete&&img.naturalWidth>0)).toBe(true);
    const audio=player.locator('audio.music-player-audio');
    await nativePlay(audio,info);
    const stream=new URL(`/music/assets/${assetId}/stream`,assetUrl).href;
    const range=await page.request.get(stream,{headers:{Range:'bytes=0-31'}});
    expect(range.status()).toBe(206);
    expect((await range.body()).length).toBe(32);
    // Browsing the main window cannot replace or pause the independent player.
    const before=await audio.evaluate(a=>a.currentTime);
    await page.goto('/music');
    await expect(page.getByRole('heading',{name:'Music',exact:true})).toBeVisible();
    await expect.poll(()=>audio.evaluate(a=>a.currentTime)).toBeGreaterThan(before);
    expect(await audio.evaluate(a=>a.paused)).toBe(false);
    if(!noScript){
      // Adding another selection to an open player must retain the media node.
      await audio.evaluate(a=>{a.dataset.qaContinuity='same-node';});
      await page.goto(assetUrl);
      await page.getByText('Queue options',{exact:true}).press('Enter');
  await page.getByRole('link',{name:'Open in music player',exact:true}).press('Enter');
      await expect(player.getByRole('button',{name:'Add to queue',exact:true})).toBeVisible();
      await expect(audio).toHaveAttribute('data-qa-continuity','same-node');
      expect(await audio.evaluate(a=>a.paused)).toBe(false);
      await command(player,'Add to queue');
      await expect(entries).toHaveCount(2);
      await expect(audio).toHaveAttribute('data-qa-continuity','same-node');
      // Pause through native controls; the enhancer durably saves the position.
      const paused=player.waitForResponse(r=>r.request().method()==='POST'&&r.request().postData()?.includes('action=seek'));
      await audio.evaluate(a=>a.pause());
      expect((await paused).status()).toBe(200);
      await expect.poll(()=>player.locator('input[name="seconds"]').inputValue()).not.toBe('0');
      expect(await player.getByLabel('Position in seconds',{exact:true}).evaluate(input=>input.checkValidity())).toBe(true);
    }
    const settings=player.locator('form.music-player-command').filter({has:player.locator('select[name="repeat_mode"]')});
    await openDetailsFor(settings);
    await settings.getByLabel('Repeat',{exact:true}).selectOption('one');
    await settings.getByLabel('Shuffle',{exact:true}).check();
    await submitForm(player,settings.getByRole('button',{name:'Save player settings',exact:true}));
    await expect(player.getByLabel('Repeat',{exact:true})).toHaveValue('one');
    await expect(player.getByLabel('Shuffle',{exact:true})).toBeChecked();
    const seek=player.locator('form.music-player-command').filter({has:player.locator('input[name="seconds"]')});
    await openDetailsFor(seek);
    await seek.getByLabel('Position in seconds',{exact:true}).fill('2.12345');
    await submitForm(player,seek.getByRole('button',{name:'Go to position',exact:true}));
    await expect(player.getByLabel('Position in seconds',{exact:true})).toHaveValue('2.12345');
    await player.reload();
    await expect(entries).toHaveCount(noScript?1:2);
    await expect(player.getByLabel('Repeat',{exact:true})).toHaveValue('one');
    await expect(player.getByLabel('Shuffle',{exact:true})).toBeChecked();
    await expect(player.getByLabel('Position in seconds',{exact:true})).toHaveValue('2.12345');
    if(info.project.name.endsWith('mobile')){await player.setViewportSize({width:320,height:800});}
    await player.getByRole('heading',{name:'Music player',exact:true}).click();
    await audit(player,noScript);
    await player.screenshot({path:info.outputPath('music-player-queue.png'),fullPage:true});
    await page.goto('/music/history');
    await page.getByText('Listening preferences',{exact:true}).press('Enter');
    await page.getByLabel('Keep my listening history',{exact:true}).check();
    await page.getByLabel('Minimum playback seconds',{exact:true}).fill('2');
    await page.getByLabel('Minimum playback percentage',{exact:true}).fill('100');
    await Promise.all([page.waitForNavigation({waitUntil:'domcontentloaded'}),page.getByRole('button',{name:'Save listening preferences',exact:true}).press('Enter')]);
    if(!noScript){
      await command(player,'Play this track');
      await nativePlay(audio,info);
      await expect(player.locator('.music-history-feedback')).toHaveText('Listen recorded.',{timeout:15000});
      await audio.evaluate(a=>a.pause());
      await page.goto('/music/history');
      await expect(page.locator('section.music-history ol > li')).not.toHaveCount(0);
      await expect(page.locator('section.music-history ol')).toContainText(title);
    }else{
      await expect(page.locator('section.music-history ol > li')).toHaveCount(0);
    }
    await audit(page,noScript);
    await page.screenshot({path:info.outputPath('music-listening-history.png'),fullPage:true});
    await statistics(page,info,noScript,title);
    await page.getByText('Listening preferences',{exact:true}).press('Enter');
    await page.getByLabel('Keep my listening history',{exact:true}).uncheck();
    await Promise.all([page.waitForNavigation({waitUntil:'domcontentloaded'}),page.getByRole('button',{name:'Save listening preferences',exact:true}).press('Enter')]);
    await page.getByText('Erase listening history',{exact:true}).first().press('Enter');
    await page.getByLabel('Remove all my recorded listens',{exact:true}).check();
    await Promise.all([page.waitForNavigation({waitUntil:'domcontentloaded'}),page.getByRole('button',{name:'Erase listening history',exact:true}).press('Enter')]);
    await expect(page.locator('section.music-history ol > li')).toHaveCount(0);
    await page.goto('/music/stats');
    await expect(page.locator('.music-stats-total')).toHaveAttribute('data-count','0');
    const tracks=page.locator('section.music-stats-card').filter({has:page.getByRole('heading',{name:'Most played tracks',exact:true})});
    await expect(tracks).not.toContainText(title);

    await page.goto('/music/favourites');
    const savedItem=page.locator('section.music-favourites li').filter({has:page.getByRole('link',{name:title,exact:true})});
    await savedItem.getByRole('button',{name:'Unlike',exact:true}).press('Enter');
    await expect(page.locator('section.music-favourites ul')).not.toContainText(title);
    await page.goto(assetUrl);
    await expect(page.getByRole('button',{name:'Like',exact:true})).toBeVisible();
    await command(player,'Clear queue');
    await expect(entries).toHaveCount(0);
    await expect(player.locator('.music-player-now')).toHaveText('The queue is empty.');
    await expect(player.locator('img.music-player-artwork')).toBeHidden();
    await audit(player,noScript);
    await player.screenshot({path:info.outputPath('music-player-empty.png'),fullPage:true});
  }finally{await player.close();}
}
test('music player keeps playing while browsing and restores its queue',async({page},info)=>journey(page,info,false));
test('native music queue controls work without JavaScript @no-js',async({page},info)=>journey(page,info,true));
