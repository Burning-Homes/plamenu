import {freshMusicAudio, playNativeAudio} from '../session.mjs';
import {test,expect} from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import {signIn} from '../session.mjs';
import {randomUUID} from 'node:crypto';

async function audit(page,noScript){
  const geometry=await page.evaluate(()=>({width:innerWidth,scroll:document.documentElement.scrollWidth,overflow:[...document.querySelectorAll('*')].filter(e=>e.getBoundingClientRect().width>0&&e.getBoundingClientRect().right>innerWidth+1).map(e=>({tag:e.tagName,class:e.className,right:e.getBoundingClientRect().right,text:e.textContent.slice(0,60)}))}));
  expect(geometry.scroll,JSON.stringify(geometry)).toBeLessThanOrEqual(geometry.width);
  if(!noScript){
    // Audit the rendered state after the feed's entrance animation completes.
    // Measuring its intermediate opacity reports false contrast violations.
    await page.evaluate(async()=>{
      await Promise.all(document.getAnimations().filter(a=>a.effect?.getTiming().iterations!==Infinity).map(a=>a.finished.catch(()=>{})));
    });
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
    expect(result.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
}
async function journey(page,info,noScript){
  test.setTimeout(180000);
  await signIn(page);
  await page.goto('/music');
  await expect(page.getByRole('heading',{name:'Music',exact:true})).toBeVisible();
  await page.getByText('Music subscriptions',{exact:true}).press('Enter');
  const publisher=info.project.name==='chromium-no-javascript'?'a11y_chromium_desktop':'a11y_chromium_no_javascript';
  const account=await page.request.get('/api/v1/accounts/lookup?acct='+publisher);
  expect(account.ok()).toBe(true);
  const publisherProfile=await account.json();
  const library=publisherProfile.uri+'/library';
  await page.getByLabel('Music library or track URL').fill(library);
  await page.getByRole('button',{name:'Subscribe to music',exact:true}).press('Enter');
  await page.getByText('Music subscriptions',{exact:true}).press('Enter');
  const subscription=page.locator('li').filter({has:page.locator('input[name="url"][value="'+library+'"]')});
  // Other journeys can legitimately change a publisher's display name.
  // The library URL above identifies the account independently of its label.
  await expect(subscription).toContainText(publisherProfile.display_name||publisherProfile.username);
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('music-subscriptions.png'),fullPage:true});
  await subscription.getByRole('button',{name:'Unsubscribe',exact:true}).press('Enter');
  await expect(page.locator('input[name="url"][value="'+library+'"]')).toHaveCount(0);
  await page.getByRole('link',{name:'New post',exact:true}).first().press('Enter');
  await page.locator('select[name="post_kind"]').selectOption('audio');
  if(noScript) await page.locator('button[name="op"][value="change_kind"]').press('Enter');
  await expect(page.locator('select[name="post_kind"]')).toHaveValue('audio');
  const title='Music QA '+randomUUID();
  await page.locator('input[name="title"]').fill(title);
  await page.getByText('More music details (optional)',{exact:true}).press('Enter');
  await page.getByLabel('Artist',{exact:true}).fill('QA artist <safe>');
  await page.getByLabel('Album',{exact:true}).fill('Federation sessions');
  await page.getByLabel('Genre',{exact:true}).fill('Ambient');
  await page.getByText('More music details (optional)',{exact:true}).press('Enter');
  await page.locator('textarea[name="status"]').fill('A synthetic tone for music publication QA.');
  if(!noScript) await expect(page.locator('.compose__dropzone')).toBeVisible();
  const upload=noScript ? page.locator('input[type="file"][name="media[]"]').first() : page.locator('.compose__media-body > input[type="file"].visually-hidden');
  await upload.setInputFiles(await freshMusicAudio());
  if (!noScript) await page.locator('.compose__attachment-details > summary').first().click();
  await page.locator(noScript?'textarea[name="media_alt[]"],input[name="media_alt[]"]':'.compose__attachment .compose__alt').first().fill('Synthetic tone; no speech or lyrics.');
  await expect(page.locator('[data-compose-schedulable]').first()).toBeHidden();
  if(!noScript){
    await page.locator('[data-compose-preview-btn]').press('Enter');
    const preview=page.locator('[data-compose-preview] audio');
    await expect(preview).toHaveCount(1);
    await expect.poll(()=>preview.evaluate(a=>a.readyState)).toBeGreaterThan(0);
    if(info.project.name.startsWith('webkit')){
      const box=await preview.boundingBox();
      // WebKit's loaded desktop player places Play after its 15-second
      // rewind control; its narrow player uses the compact layout.
      await preview.click({position:{x:info.project.name==='webkit-desktop'?56:20,y:box.height/2}});
    }else await preview.press('Space');
    await expect.poll(()=>preview.evaluate(a=>a.currentTime)).toBeGreaterThan(0);
    expect(await preview.evaluate(a=>a.error?.code||null)).toBeNull();
  }
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('audio-composer.png'),fullPage:true});
  const submit=page.locator('button[type="submit"][name="op"][value="post"]');
  await submit.press('Enter');
  await expect(page).toHaveURL(url=>/^\/@[^/]+\/\d+$/.test(url.pathname));
  await expect(page.locator('audio')).toHaveCount(1);
  await expect(page.locator('.status__music')).toContainText('QA artist <safe>');
  await expect(page.locator('.status__music')).toContainText('Federation sessions');
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('audio-publication.png'),fullPage:true});
  const publication=page.url();
  // The attachment is the only primary player; specialist actions start closed.
  await expect(page.locator('.status form[action="/web/music/play"]')).toHaveCount(0);
  await expect(page.getByRole('button',{name:'Like',exact:true})).toBeHidden();
  await expect(page.getByRole('link',{name:'Open in music player',exact:true})).toBeHidden();
  await playNativeAudio(page.locator('audio'),info);
  await expect.poll(()=>page.locator('audio').evaluate(a=>a.currentTime),{timeout:15000}).toBeGreaterThan(0);
  expect(await page.locator('audio').evaluate(a=>a.error?.code||null)).toBeNull();
  await page.goto('/music?mine=true');
  await page.getByRole('searchbox',{name:'Search music or paste a link'}).fill(title);
  await page.getByRole('searchbox',{name:'Search music or paste a link'}).press('Enter');
  await expect(page.locator('audio')).toHaveCount(1);
  await expect(page.locator('.status__music')).toContainText('QA artist <safe>');
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('music-catalogue.png'),fullPage:true});
  await page.goto(publication);
  await page.locator('summary[title="More options"]').first().press('Enter');
  await page.getByRole('link',{name:'Edit',exact:true}).press('Enter');
  await page.getByLabel('Artist',{exact:true}).fill('Edited QA artist');
  await page.getByLabel('Release year',{exact:true}).fill('2026');
  await page.locator('button[name="op"][value="preview"]').press('Enter');
  await expect(page.locator('.status__music')).toContainText('Edited QA artist');
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('audio-edit.png'),fullPage:true});
  await page.locator('button[name="op"][value="save"]').press('Enter');
  await expect(page.locator('.status__music')).toContainText('Edited QA artist');
  await page.goto(publication);
  const audio=page.locator('audio');
  if(info.project.name.startsWith('webkit')){
    const box=await audio.boundingBox();
    await audio.click({position:{x:20,y:box.height/2}});
  }else await audio.press('Space');
  await expect.poll(()=>audio.evaluate(a=>a.currentTime),{timeout:15000}).toBeGreaterThan(0);
  expect(await audio.evaluate(a=>a.error?.code||null)).toBeNull();
}

test('Audio publication and music discovery are accessible',async({page},info)=>journey(page,info,false));
test('Audio publication and music discovery work without JavaScript @no-js',async({page},info)=>journey(page,info,true));
