import {test,expect} from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import {signIn,openDetailsFor,ensureVideoFormChannel} from '../session.mjs';
const source=process.env.A11Y_SOURCE_SYNC_URL;
async function audit(page,noScript){
  await page.setViewportSize({width:320,height:800});
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
  if(!noScript){
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
    expect(result.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
}
async function journey(page,info,noScript){
  test.setTimeout(240000);expect(source,'Set A11Y_SOURCE_SYNC_URL to the disposable public PeerTube channel/playlist').toBeTruthy();
  await signIn(page);await page.goto('/videos');
  await page.getByRole('link',{name:'Scheduled sources',exact:true}).press('Enter');
  let form=page.locator('form[action="/web/videos/sources"]');
  await ensureVideoFormChannel(page,form);
  if(await page.locator('#source-create').getAttribute('open')===null)await page.locator('#source-create > summary').press('Enter');
  await form.getByLabel('ActivityPub channel or playlist URL',{exact:true}).fill('https://user:secret@example.org/source');
  await form.getByRole('combobox',{name:'Check interval',exact:true}).selectOption('15');
  const rights=form.getByRole('checkbox',{name:'I authorize periodic retrieval and have permission to copy these videos. I will review each draft before publishing it.',exact:true});
  await form.getByRole('button',{name:'Add scheduled source',exact:true}).press('Enter');await expect(rights).toBeFocused();
  await rights.press('Space');await form.getByRole('button',{name:'Add scheduled source',exact:true}).press('Enter');
  await expect(page.getByRole('alert')).toContainText('Use a URL without credentials or a fragment');
  form=page.locator('form[action="/web/videos/sources"]');await expect(form.getByLabel('ActivityPub channel or playlist URL',{exact:true})).toHaveValue('https://user:secret@example.org/source');
  await form.getByLabel('ActivityPub channel or playlist URL',{exact:true}).fill(source);await audit(page,noScript);
  await form.getByRole('checkbox').press('Space');await form.getByRole('button',{name:'Add scheduled source',exact:true}).press('Enter');
  await expect(page.getByText(source,{exact:true})).toBeVisible();
  const pause=page.getByRole('button',{name:'Pause discovery',exact:true});const action=await pause.locator('..').getAttribute('action');
  const id=action.split('/').at(-2);
  try{
    const section=page.locator(`[aria-labelledby="source-title-${id}"]`);
    await pause.press('Enter');await expect(section.getByRole('status')).toContainText('Paused');
    await audit(page,noScript);await page.screenshot({path:info.outputPath('sources-paused-320px.png'),fullPage:true});
    if(!noScript)await page.route('**/videos/sources/state',route=>route.fulfill({status:503,body:'Temporarily unavailable'}));
    await section.getByRole('button',{name:'Resume discovery',exact:true}).press('Enter');
    if(!noScript){
      await expect(page.locator('[data-source-refresh-error]')).toContainText('Automatic status refresh failed');
      await page.unroute('**/videos/sources/state');
    }
    await expect.poll(async()=>{
      if(noScript)await page.getByRole('link',{name:'Refresh source status',exact:true}).press('Enter');
      return page.locator(`[data-source-state="${id}"]`).innerText();
    },{timeout:120000,intervals:[1000,2000,3000]}).toContain('Checked; review prepared drafts');
    await page.getByRole('link',{name:'Review prepared videos',exact:true}).press('Enter');
    // Imported titles start with the dedicated PeerTube fixture marker.
    const drafts=page.locator('a[href^="/videos/drafts/"]').filter({hasText:/Sync source/});
    await expect.poll(()=>drafts.count(),{timeout:60000}).toBeGreaterThan(0);
    const hrefs=await drafts.evaluateAll(nodes=>nodes.map(n=>n.getAttribute('href')));
    for(const href of hrefs){
      await page.goto(href);
      await expect.poll(async()=>{
        if(noScript)await page.getByRole('link',{name:'Refresh status',exact:true}).press('Enter');
        return page.getByRole('status').innerText();
      },{timeout:120000,intervals:[1000,2000,3000]}).toBe('Ready to review');
      await page.getByRole('link',{name:'Review your video',exact:true}).press('Enter');
      const video=page.locator('video.video-publish__preview');
      await video.evaluate(async v=>{v.muted=true;await v.play();});
      await expect.poll(()=>video.evaluate(v=>v.currentTime)).toBeGreaterThan(.1);
      await expect.poll(()=>video.evaluate(v=>v.videoWidth)).toBeGreaterThan(0);
      await video.evaluate(v=>v.pause());await audit(page,noScript);
      await page.screenshot({path:info.outputPath('sync-private-review-320px.png'),fullPage:true});
      await page.goto(href);
      const deleteConfirmation=page.getByRole('checkbox',{name:'I want to delete this draft and its files',exact:true,includeHidden:true});
      await openDetailsFor(deleteConfirmation);await expect(deleteConfirmation).toBeVisible();
      await deleteConfirmation.press('Space');
      await page.getByRole('button',{name:'Cancel and delete draft',exact:true}).press('Enter');
    }
    await page.goto('/videos/sources');await page.getByRole('button',{name:'Pause discovery',exact:true}).press('Enter');
    const sourceSection=page.locator(`[aria-labelledby="source-title-${id}"]`);
    const removeConfirmation=sourceSection.getByRole('checkbox',{name:'Stop this source and forget its import history',exact:true,includeHidden:true});
    await openDetailsFor(removeConfirmation);await expect(removeConfirmation).toBeVisible();
    await removeConfirmation.press('Space');
    await sourceSection.getByRole('button',{name:'Remove source',exact:true}).press('Enter');
    await expect(page.getByText(source,{exact:true})).toHaveCount(0);await audit(page,noScript);
    await page.emulateMedia({colorScheme:'light',reducedMotion:'reduce',forcedColors:'active'});await audit(page,noScript);
  }finally{
    await page.goto('/videos/sources');
    const csrf=await page.locator('input[name="csrf"]').first().getAttribute('value');
    await page.request.post(`/web/videos/sources/${id}/remove`,{form:{csrf,confirm:'yes'},maxRedirects:0});
  }
}
test('scheduled source discovery prepares decoded private drafts with accessible controls',async({page},info)=>journey(page,info,false));
test('scheduled source discovery prepares decoded private drafts with accessible controls @no-js',async({page},info)=>journey(page,info,true));
