import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {randomUUID} from 'node:crypto';
import {signIn} from '../session.mjs';

async function audit(page,noScript){
  await page.setViewportSize({width:320,height:800});
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
  if(!noScript){
    for(const colorScheme of ['light','dark']){
      await page.emulateMedia({colorScheme,reducedMotion:'reduce'});
      // Wait for the requested WebKit style recalculation, not just for the
      // media-emulation command acknowledgement before cloning the Axe frame.
      await expect.poll(()=>page.evaluate(()=>getComputedStyle(document.body).color))
        .toBe(colorScheme==='dark'?'rgb(233, 237, 242)':'rgb(21, 25, 30)');
      await expect.poll(()=>page.evaluate(()=>[...new Set([...document.querySelectorAll('h1,legend,.settings-field__label')].map(e=>getComputedStyle(e).color))]))
        .toEqual([colorScheme==='dark'?'rgb(233, 237, 242)':'rgb(21, 25, 30)']);
      const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
      expect(result.violations.map(v=>({id:v.id,scheme:colorScheme,nodes:v.nodes.map(n=>({target:n.target,summary:n.failureSummary}))}))).toEqual([]);
    }
    await page.emulateMedia({colorScheme:null,reducedMotion:null});
  }
}
async function journey(page,info,noScript){
  test.setTimeout(120000);await signIn(page);await page.goto('/videos');
  const channel=page.locator('form[action="/web/video-channels"]');
  if(!await channel.isVisible())await channel.locator('..').locator(':scope > summary').press('Enter');
  const name=`live_${randomUUID().replaceAll('-','').slice(0,20)}`;
  await channel.getByLabel('Channel address').fill(name);await channel.getByLabel('Channel name').fill(`Live ${name}`);
  const [created]=await Promise.all([page.waitForResponse(r=>new URL(r.url()).pathname==='/web/video-channels'&&r.request().method()==='POST'),channel.getByRole('button',{name:'Create a video channel',exact:true}).press('Enter')]);
  expect(created.status()).toBe(303);
  await page.goto('/videos/live/new');
  let form=page.locator('form[action="/web/videos/live"]');
  await form.getByRole('combobox',{name:'Channel',exact:true}).selectOption({label:`Live ${name}`});
  await form.getByLabel('Video title',{exact:true}).fill(`Warning-bearing live ${name}`);
  await form.getByLabel('Description',{exact:true}).fill('A planned live test pattern.');
  await form.getByLabel('Language code',{exact:false}).fill('en');
  await form.getByRole('combobox',{name:'Visibility',exact:true}).selectOption('unlisted');
  await form.getByRole('combobox',{name:'Category',exact:true}).selectOption('15');
  await form.getByRole('combobox',{name:'Licence',exact:true}).selectOption('2');
  await form.getByLabel('Content warning',{exact:true}).fill('Flashing test imagery');
  await form.getByRole('checkbox',{name:'Keep this broadcast reusable after the encoder disconnects',exact:true}).press('Space');
  // An otherwise valid form with a past planned start must preserve the
  // authored fields, channel, terms and warning for ordinary HTML correction.
  await form.locator('input[name="scheduled_at"]').fill('2020-01-01T12:00');
  await audit(page,noScript);
  const [invalid]=await Promise.all([page.waitForResponse(r=>new URL(r.url()).pathname==='/web/videos/live'&&r.request().method()==='POST'),form.getByRole('button',{name:'Create waiting broadcast',exact:true}).press('Enter')]);
  expect(invalid.status()).toBe(422);await expect(page.getByRole('alert')).toBeVisible();
  form=page.locator('form[action="/web/videos/live"]');
  await expect(form.getByLabel('Video title',{exact:true})).toHaveValue(`Warning-bearing live ${name}`);
  await expect(form.getByLabel('Description',{exact:true})).toHaveValue('A planned live test pattern.');
  await expect(form.getByLabel('Content warning',{exact:true})).toHaveValue('Flashing test imagery');
  await expect(form.getByRole('combobox',{name:'Category',exact:true})).toHaveValue('15');
  await expect(form.getByRole('combobox',{name:'Licence',exact:true})).toHaveValue('2');
  await expect(form.getByRole('checkbox',{name:'Keep this broadcast reusable after the encoder disconnects',exact:true})).toBeChecked();
  await form.locator('input[name="scheduled_at"]').fill('');await audit(page,noScript);
  await page.screenshot({path:info.outputPath('live-metadata-correction-320px.png'),fullPage:true});
  await form.getByRole('button',{name:'Create waiting broadcast',exact:true}).press('Enter');
  await expect(page.getByRole('textbox',{name:'Stream key',exact:true})).toBeVisible();
  // No screenshot of the one-time secret response: protected evidence must
  // not accidentally contain a still-valid ingest credential.
  const href=await page.getByRole('link',{name:'Manage live broadcast',exact:true}).getAttribute('href');
  const id=href.split('/').pop();
  try{
    await page.goto(href);await expect(page.getByText('The stream key is hidden after creation',{exact:false})).toBeVisible();
    const response=await page.request.get(`/api/v1/statuses/${id}`);expect(response.status()).toBe(200);
    const status=await response.json();expect(status.sensitive).toBe(true);expect(status.spoiler_text).toBe('Flashing test imagery');
    const metadata=status.media_attachments[0].video_metadata;
    expect(metadata.category.id).toBe(15);expect(metadata.licence.id).toBe(2);
    expect(metadata.licence.url).toBe('https://creativecommons.org/licenses/by-sa/4.0/');
    await audit(page,noScript);await page.screenshot({path:info.outputPath('live-metadata-manage-320px.png'),fullPage:true});
  }finally{
    await page.goto(href);
    const csrf=await page.locator('input[name="csrf"]').first().getAttribute('value');
    expect((await page.request.post(`/web/statuses/${id}/delete`,{form:{csrf},maxRedirects:0})).status()).toBe(303);
  }
}
test('live creation terms and warning survive accessible HTML validation',async({page},info)=>journey(page,info,false));
test('live creation terms and warning survive accessible HTML validation @no-js',async({page},info)=>journey(page,info,true));
