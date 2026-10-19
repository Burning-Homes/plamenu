import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {randomUUID} from 'node:crypto';
import {signIn,openVideoOptions,createVideoChannel,videoChannelForm,chooseVideoLanguage} from '../session.mjs';

async function check(page,noScript){
  await page.setViewportSize({width:320,height:800});
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=document.documentElement.clientWidth)).toBe(true);
  if(!noScript){
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
    expect(result.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
}
async function ready(page,noScript){
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
async function decoded(video,duration){
  await video.evaluate(v=>{v.muted=true;v.play().catch(()=>{});});
  await expect.poll(()=>video.evaluate(v=>v.currentTime),{timeout:25000}).toBeGreaterThan(.2);
  await expect.poll(()=>video.evaluate(v=>v.videoWidth)).toBeGreaterThan(0);
  expect(await video.evaluate(v=>v.duration)).toBeGreaterThan(duration-.1);
  // The existing fMP4 copy-top has two 10fps B-frame composition offsets.
  // Its MediaSource timeline is 0.2s longer than the MP4/EXTINF duration,
  // without extra decoded frames. Keep private/native MP4 checks strict;
  // real frame-by-frame cut boundaries are separately asserted in Rust.
  const mse=await video.evaluate(v=>v.currentSrc.startsWith('blob:'));
  expect(await video.evaluate(v=>v.duration)).toBeLessThan(duration+(mse ? 0.25 : 0.1));
  await video.evaluate(v=>v.pause());
}
async function apply(page,permalink){
  const button=page.getByRole('button',{name:'Apply the reviewed replacement',exact:true});
  const confirm=page.getByRole('checkbox',{name:'I reviewed the replacement and want to apply it to this post',exact:true});
  await button.press('Enter');await expect(confirm).toBeFocused();
  await confirm.press('Space');await button.press('Enter');await page.waitForURL(permalink);
}
async function journey(page,info,noScript){
  test.setTimeout(300000);
  if(noScript) await page.setViewportSize({width:320,height:800});
  await signIn(page);await page.goto('/videos/upload');await openVideoOptions(page);
  const channelForm=await videoChannelForm(page);
  if(!await channelForm.isVisible())await channelForm.locator('..').locator(':scope > summary').press('Enter');
  const name=`source_${randomUUID().replaceAll('-','').slice(0,20)}`;
  const channel=`Source ${name}`;
  await channelForm.getByLabel('Channel address').fill(name);await channelForm.getByLabel('Channel name').fill(channel);
  await createVideoChannel(page,channelForm.getByRole('button',{name:'Create a video channel',exact:true}));
  await openVideoOptions(page);
  const upload=page.locator('form[action="/web/videos"]');
  await upload.getByRole('combobox',{name:'Channel',exact:true}).selectOption({label:channel});
  await upload.getByLabel('Video title',{exact:true}).fill(`Film ${name}`);
  await chooseVideoLanguage(upload,'en');
  await upload.getByLabel('Visual description',{exact:true}).fill('Moving test pattern');
  await upload.getByLabel('Transcript',{exact:true}).fill('Steady tone');
  await upload.getByLabel('Captions (WebVTT)',{exact:true}).setInputFiles({name:'source.vtt',mimeType:'text/vtt',buffer:Buffer.from('WEBVTT\n\n00:00:00.000 --> 00:00:03.000\n[Steady tone]\n')});
  await upload.getByLabel('Video or audio file',{exact:true}).setInputFiles(process.env.A11Y_VIDEO_FIXTURE);
  await upload.getByRole('button',{name:'Upload video',exact:true}).press('Enter');
  const original=await ready(page,noScript);
  await page.getByRole('button',{name:'Publish video',exact:true}).press('Enter');
  await page.waitForURL(url=>/\/@[^/]+\/\d+/.test(url.pathname));const permalink=page.url();
  await decoded(page.locator('video').first(),3);
  await page.goto(`/videos/${original}/edit`);await check(page,noScript);
  await page.locator('summary').filter({hasText:/^Edit the retained source$/}).press('Enter');
  await page.getByRole('button',{name:'Prepare reprocessed video',exact:true}).press('Enter');
  const reprocessed=await ready(page,noScript);
  await expect(page.getByText('This is a private edit candidate, not a new post.',{exact:false})).toBeVisible();
  await expect(page.getByRole('button',{name:'Publish video',exact:true})).toHaveCount(0);
  await expect(page.getByLabel('Visual description',{exact:true})).toHaveValue('Moving test pattern');
  await decoded(page.getByLabel('Video preview',{exact:true}),3);
  await check(page,noScript);await page.screenshot({path:info.outputPath('reprocess-review-320px.png'),fullPage:true});
  await page.getByRole('button',{name:'Save alternatives and review edit',exact:true}).press('Enter');
  await page.waitForURL(url=>url.pathname===`/videos/${original}/edit`&&url.searchParams.get('candidate')===reprocessed);
  await apply(page,permalink);
  expect(await page.locator('video').first().getAttribute('data-hls')).toContain('rev=1');
  await decoded(page.locator('video').first(),3);
  await page.goto(`/videos/${original}/edit`);await page.getByText('Trim the published film',{exact:true}).press('Enter');
  await page.getByLabel('Start time (seconds)',{exact:true}).fill('.5');
  await page.getByLabel('End time (seconds)',{exact:true}).fill('2.5');
  await page.getByRole('button',{name:'Prepare private trim',exact:true}).press('Enter');
  const trimmed=await ready(page,noScript);
  await expect(page.getByLabel('Visual description',{exact:true})).toHaveValue('');
  await expect(page.getByLabel('Transcript',{exact:true})).toHaveValue('');
  const captions=await page.request.get(`/videos/drafts/${trimmed}/captions/en.vtt`);
  expect(captions.status()).toBe(200);expect(await captions.text()).toContain('00:00:00.000 --> 00:00:02.000');
  await decoded(page.getByLabel('Video preview',{exact:true}),2);
  await page.getByLabel('Visual description',{exact:true}).fill('Selected moving test pattern');
  await page.getByLabel('Transcript',{exact:true}).fill('Selected steady tone');
  await check(page,noScript);await page.screenshot({path:info.outputPath('trim-alternatives-320px.png'),fullPage:true});
  await page.getByRole('button',{name:'Save alternatives and review edit',exact:true}).press('Enter');
  await page.waitForURL(url=>url.pathname===`/videos/${original}/edit`&&url.searchParams.get('candidate')===trimmed);
  await page.getByText('Visual description',{exact:true}).press('Enter');
  await expect(page.getByText('Selected moving test pattern',{exact:true})).toBeVisible();
  await check(page,noScript);await apply(page,permalink);
  expect(await page.locator('video').first().getAttribute('data-hls')).toContain('rev=2');
  await decoded(page.locator('video').first(),2);await expect(page.getByText(`Film ${name}`,{exact:true})).toBeVisible();
  await check(page,noScript);await page.screenshot({path:info.outputPath('trim-published-320px.png'),fullPage:true});
  expect((await page.request.get(`/videos/drafts/${trimmed}/review`)).status()).toBe(404);
}
test('retained-source reprocess and trim are private, decoded, accessible and explicitly applied',async({page},info)=>journey(page,info,false));
test('retained-source reprocess and trim are private, decoded, accessible and explicitly applied @no-js',async({page},info)=>journey(page,info,true));
