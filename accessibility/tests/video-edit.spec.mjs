import {test,expect} from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import {randomUUID} from 'node:crypto';
import {signIn,openVideoOptions,createVideoChannel,videoChannelForm,chooseVideoLanguage} from '../session.mjs';

async function check(page,noScript){
  await page.setViewportSize({width:320,height:800});
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
  if(!noScript){
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
    expect(result.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
}
async function ready(page,channel,file,title,noScript){
  await page.goto('/videos/upload');await openVideoOptions(page);
  const form=page.locator('form[action="/web/videos"]');
  await form.getByRole('combobox',{name:'Channel',exact:true}).selectOption({label:channel});
  await form.getByLabel('Video title',{exact:true}).fill(title);
  await chooseVideoLanguage(form,'en');
  await form.getByLabel('Visual description',{exact:true}).fill(`Visual alternative for ${title}`);
  await form.getByLabel('Transcript',{exact:true}).fill(`Spoken alternative for ${title}`);
  await form.getByLabel('Video or audio file',{exact:true}).setInputFiles(file);
  await form.getByLabel('Captions (WebVTT)',{exact:true}).setInputFiles({name:'replacement.vtt',mimeType:'text/vtt',
    buffer:Buffer.from(`WEBVTT\n\n00:00:00.000 --> 00:00:03.000\n[${title}]\n`)});
  await form.getByRole('button',{name:'Upload video',exact:true}).press('Enter');
  await page.waitForURL(url=>/\/videos\/drafts\/\d+$/.test(url.pathname));
  const id=new URL(page.url()).pathname.split('/').pop();
  if(noScript){
    for(let i=0;i<60;i++){
      if(await page.getByRole('link',{name:'Review your video',exact:true}).isVisible())break;
      await page.waitForTimeout(1000);
      await page.getByRole('link',{name:'Refresh status',exact:true}).press('Enter');
    }
  }
  await expect(page.getByRole('link',{name:'Review your video',exact:true})).toBeVisible({timeout:60000});
  await page.getByRole('link',{name:'Review your video',exact:true}).press('Enter');
  return id;
}
async function decoded(video){
  await video.evaluate(v=>{v.muted=true;v.play().catch(()=>{});});
  await expect.poll(()=>video.evaluate(v=>v.currentTime),{timeout:25000}).toBeGreaterThan(.2);
  await expect.poll(()=>video.evaluate(v=>v.videoWidth)).toBeGreaterThan(0);
  await video.evaluate(v=>v.pause());
}
async function journey(page,info,noScript){
  test.setTimeout(240000);
  await signIn(page);
  await page.goto('/videos/upload');await openVideoOptions(page);
  const form=await videoChannelForm(page);
  if(!await form.isVisible())await form.locator('..').locator(':scope > summary').press('Enter');
  const name=`edit_${randomUUID().replaceAll('-','').slice(0,20)}`;
  const channel=`Edit ${name}`;
  await form.getByLabel('Channel address').fill(name);
  await form.getByLabel('Channel name').fill(channel);
  const [creation] = await Promise.all([
    page.waitForResponse(response=>new URL(response.url()).pathname==='/web/video-channels'
      && response.request().method()==='POST'),
    form.getByRole('button',{name:'Create a video channel',exact:true}).press('Enter'),
  ]);
  // The form returns to the same URL: a URL-only waiter can resolve before
  // WebKit submits and the next goto aborts the not-yet-committed creation.
  expect(creation.status()).toBe(303);
  const original=await ready(page,channel,process.env.A11Y_VIDEO_FIXTURE,`Original ${name}`,noScript);
  await page.locator('form[action="/web/videos"]').getByRole('button',{name:'Publish video',exact:true}).press('Enter');
  await page.waitForURL(url=>/\/@[^/]+\/\d+/.test(url.pathname));
  const permalink=page.url();
  const oldSource=await page.locator('video').first().getAttribute('data-hls');
  await decoded(page.locator('video').first());
  const candidate=await ready(page,channel,process.env.A11Y_AUDIO_FIXTURE,`Replacement ${name}`,noScript);
  await page.goto(`/videos/${original}/metadata`);
  await page.getByRole('link',{name:'Edit the published film',exact:true}).press('Enter');
  await expect(page.getByRole('heading',{name:'Edit the published film',exact:true})).toBeVisible();
  const selection=page.getByRole('combobox',{name:'Replacement draft',exact:true});
  await selection.selectOption(candidate);
  await page.getByRole('button',{name:'Review your video',exact:true}).press('Enter');
  await expect(page.getByRole('heading',{name:'Private replacement preview',exact:true})).toBeVisible();
  await page.getByText('Visual description',{exact:true}).press('Enter');
  await expect(page.getByText(`Visual alternative for Replacement ${name}`,{exact:true})).toBeVisible();
  await page.getByText('Transcript',{exact:true}).press('Enter');
  await expect(page.getByText(`Spoken alternative for Replacement ${name}`,{exact:true})).toBeVisible();
  await decoded(page.getByLabel('Play the private replacement film',{exact:true}));
  const preview=page.getByLabel('Play the private replacement film',{exact:true});
  await expect(preview.locator('track[kind="captions"]')).toHaveCount(1);
  await check(page,noScript);
  await page.screenshot({path:info.outputPath('replacement-review-320px.png'),fullPage:true});
  const apply=page.getByRole('button',{name:'Apply the reviewed replacement',exact:true});
  const confirm=page.getByRole('checkbox',{name:'I reviewed the replacement and want to apply it to this post',exact:true});
  await apply.press('Enter');
  await expect(confirm).toBeFocused();
  await confirm.press('Space');
  await apply.press('Enter');
  await page.waitForURL(permalink);
  await expect(page.getByText(`Original ${name}`,{exact:true})).toBeVisible();
  const film=page.locator('video').first();
  expect(await film.getAttribute('data-hls')).toContain('rev=1');
  expect(await film.getAttribute('data-hls')).not.toBe(oldSource);
  await decoded(film);
  await expect.poll(()=>film.evaluate(v=>v.duration)).toBeGreaterThan(8);
  await check(page,noScript);
  await page.screenshot({path:info.outputPath('replacement-published-320px.png'),fullPage:true});
  expect((await page.request.get(`/videos/drafts/${candidate}/review`)).status()).toBe(404);
  // Follow actual management links and return without losing the publication.
  await page.goto(`/videos/${original}/manage`);
  for (const name of ['Manage category and licence','Manage captions','Video chapters','Downloads and embeds','Edit the published film']) {
    await page.locator('.video-workspace__tools').getByRole('link',{name,exact:false}).press('Enter');
    const back=name==='Downloads and embeds'
      ? page.getByRole('link',{name:'Return to your video',exact:true})
      : page.locator('.video-workspace__nav').getByRole('link',{name:'Manage video',exact:true});
    await expect(back).toBeVisible();
    await check(page,noScript);
    await back.press('Enter');
    await expect(page).toHaveURL(url=>url.pathname===`/videos/${original}/manage`);
  }

}
test('published-film replacement is private, decoded, confirmed and identity-preserving',async({page},info)=>journey(page,info,false));
test('published-film replacement is private, decoded, confirmed and identity-preserving @no-js',async({page},info)=>journey(page,info,true));
