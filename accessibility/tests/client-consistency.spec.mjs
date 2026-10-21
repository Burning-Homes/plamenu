import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {signIn,signInParticipant,freshMusicAudio} from '../session.mjs';

async function audit(page, scripted = true) {
  // Axe executes page scripts; disabled-script clients still get the layout
  // and native interaction checks without enabling application JavaScript.
  if (scripted) {
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
    expect(result.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
  expect(await page.evaluate(()=>document.documentElement.scrollWidth)).toBeLessThanOrEqual(await page.evaluate(()=>document.documentElement.clientWidth));
}
async function publish(page,text) {
  await page.locator('textarea[name=status]').fill(text);
  await page.locator('.compose__publish').click();
  await expect(page).toHaveURL(/\/@[^/]+\/\d+/);
  return new URL(page.url()).pathname;
}

for(const mediaType of ['audio','video']) test(`a Note accepts new and existing ${mediaType} without changing its post type`,async({page},info)=>{
  test.setTimeout(180000);
  await signIn(page);
  await page.goto('/compose');
  await expect(page.locator('[data-compose-kind]')).toHaveValue('note');
  await expect(page.locator('.compose__dropzone')).toBeVisible();
  await page.locator('.compose__media-body > input[type=file]').setInputFiles(mediaType==='audio'?await freshMusicAudio():process.env.A11Y_VIDEO_FIXTURE);
  await expect(page.locator('.compose__attachment '+mediaType)).toBeVisible();
  await expect(page.locator('.compose__attachment textarea[name="media_transcript[]"]')).toBeHidden();
  await page.locator('textarea[name=status]').fill('Listen to the recording attached to this ordinary note.');
  await page.getByRole('button',{name:'Preview',exact:true}).click();
  await expect(page.locator('[data-compose-preview] '+mediaType)).toBeVisible();
  const title='Note '+mediaType+' '+Date.now().toString(36);
  const original=await publish(page,title);
  await expect(page.locator('.status '+mediaType)).toBeVisible();
  await page.goto('/compose');
  await page.getByText('Choose from your uploads',{exact:true}).click();
  const choice=page.locator('.compose__existing-list label').filter({hasText:title}).first();
  await expect(choice).toBeVisible();
  await choice.getByRole('checkbox').check();
  await page.getByRole('button',{name:'Preview',exact:true}).click();
  await expect(page.locator('[data-compose-preview] '+mediaType)).toBeVisible();
  const reuse=await publish(page,'A second Note using the same recording.');
  expect(reuse).not.toBe(original);
  await expect(page.locator('.status '+mediaType)).toBeVisible();
  const playback=page.locator('.status '+mediaType).first();
  await playback.evaluate(el=>{el.muted=true;return el.play();});
  await expect.poll(()=>playback.evaluate(el=>el.currentTime)).toBeGreaterThan(.1);
  expect(await playback.evaluate(el=>el.error?.code??null)).toBeNull();
  await playback.evaluate(el=>el.pause());
  for(const width of [320,390,768,1440]){
    await page.setViewportSize({width,height:900});
    await page.screenshot({path:info.outputPath(`note-${mediaType}-${width}.png`),fullPage:true,animations:'disabled',timeout:30000});
    await audit(page);
  }
});

for (const prefix of ['', '@no-js ']) test(`${prefix}notifications and messages form distinct readable inboxes with synchronized read actions`,async({page,browser},info)=>{
  test.setTimeout(180000);
  await signInParticipant(page);
  const me={username:(await page.locator('.account__handle').first().textContent()).trim().replace(/^@/,'')};
  expect(me.username).not.toBe('');
  const senderContext=await browser.newContext({...info.project.use,baseURL:process.env.A11Y_BASE_URL,javaScriptEnabled:!prefix});
  const sender=await senderContext.newPage();
  try {
    await signIn(sender);
    await sender.goto('/compose');
    await publish(sender,`@${me.username} Can you share that song?`);
    await sender.goto('/compose?visibility=direct');
    await expect(sender.locator('[name=recipients]')).toHaveCount(0);
    const privateText='Private listening plans '+Date.now().toString(36);
    const thread=await publish(sender,`@${me.username} ${privateText}`);
    await page.goto('/notifications');
    await expect(page.locator('.notification__label').filter({hasText:'mentioned you'})).toHaveCount(2);
    await expect(page.locator('main')).toContainText(privateText);
    await expect(page.locator('.notification-inbox-actions, .notification-inbox-item, .inbox-tabs')).toHaveCount(0);
    await expect(page.locator('[data-notification-nav] .bell__dot')).toHaveCount(0);
    await page.locator('.inbox-options > summary').click();
    await expect(page.getByRole('link',{name:'Filtered notifications',exact:true})).toBeVisible();
    await expect(page.locator('.inbox-options')).not.toContainText('Opening this inbox');
    await page.locator('.inbox-options > summary').click();
    for(const width of [320,390,768,1440]){
      await page.setViewportSize({width,height:900});
      await page.screenshot({path:info.outputPath(`notifications-${width}.png`),fullPage:true,animations:'disabled',timeout:30000});
      if(width>=390){
        const line=await page.locator('.notification__label').first().evaluate(el=>({actor:el.querySelector('.notification__actor').getClientRects()[0].top,action:el.querySelector('.notification__label-detail').getClientRects()[0].top}));
        expect(Math.abs(line.actor-line.action)).toBeLessThan(3);
      }
      await audit(page, !prefix);
    }
    await expect(page.getByRole('button',{name:'Mark all read',exact:true})).toHaveCount(0);
    let unread=await (await page.request.get('/web/inbox/state')).json();
    expect(unread.notifications).toBe(false);expect(unread.messages).toBe(true);
    await page.goto('/conversations');
    await expect(page.locator('.conversation__preview').filter({hasText:privateText})).toBeVisible();
    await expect(page.locator('.conversation .status')).toHaveCount(0);
    const second=await page.context().newPage();
    await second.goto('/conversations');
    await expect(second.locator('[data-message-nav] .bell__dot').first()).toBeAttached();
    await page.locator('.conversation__open').filter({hasText:privateText}).click();
    await expect(page).toHaveURL(new RegExp(thread+'(?:#.*)?$'));
    unread=await (await page.request.get('/web/inbox/state')).json();
    expect(unread.notifications).toBe(false);expect(unread.messages).toBe(false);
    await second.bringToFront();
    if (prefix) await second.reload();
    await expect(second.locator('[data-message-nav] .bell__dot')).toHaveCount(0);
    await expect(second.locator('.conversation').filter({hasText:privateText})).not.toHaveClass(/is-unread/);
    for(const width of [320,390,768,1440]){
      await second.setViewportSize({width,height:900});
      await second.screenshot({path:info.outputPath(`messages-${width}.png`),fullPage:true,animations:'disabled',timeout:30000});
      await audit(second, !prefix);
    }
    await second.close();
  } finally {await senderContext.close();}
});
