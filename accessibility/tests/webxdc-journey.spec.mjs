import {signInParticipant,participantUsername} from '../session.mjs';
import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {fileURLToPath} from 'node:url';
import {signIn} from '../session.mjs';

const bundle=fileURLToPath(new URL('../fixtures/webxdc/a11y.xdc',import.meta.url));

async function journey(page,browser,info) {
  const scripted=info.project.use.javaScriptEnabled!==false;
  await signIn(page);
  await page.goto('/webxdc/new');
  const form=page.locator('form[data-webxdc-create]');
  await form.locator('[name=bundle]').setInputFiles({name:'broken.xdc',mimeType:'application/webxdc+zip',buffer:Buffer.from('Not a ZIP archive.')});
  await form.locator('[name=name]').fill('Accessible fixture session');
  await form.locator('[name=summary]').fill('Shared activity in a disposable local fixture.');
  await form.locator('details.webxdc-advanced > summary').press('Enter');
  await form.locator('[name=membership_policy]').selectOption('approval');
  await form.locator('[name=send_update_interval]').fill('1000');
  await form.locator('[name=send_update_max_size]').fill('4096');
  await form.locator('button[type=submit]').press('Enter');
  const error=page.locator('.webxdc-create [role=alert]');
  await expect(error).toBeVisible();
  await page.screenshot({path:info.outputPath('webxdc-create-error.png'),fullPage:true});
  await expect.soft(error).toBeFocused({timeout:1500});
  await expect(form.locator('[name=name]')).toHaveValue('Accessible fixture session');
  await expect(form.locator('[name=summary]')).toHaveValue('Shared activity in a disposable local fixture.');
  await expect(form.locator('[name=membership_policy]')).toHaveValue('approval');
  await expect(form.locator('[name=send_update_interval]')).toHaveValue('1000');
  await expect(form.locator('[name=send_update_max_size]')).toHaveValue('4096');
  if (scripted) {
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
    expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
  await form.locator('[name=bundle]').setInputFiles(bundle);
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.webxdc-page h1')).toHaveText('Accessible fixture session');
  const sessionPath=new URL(page.url()).pathname;
  const participantContext=await browser.newContext({...info.project.use,baseURL:process.env.A11Y_BASE_URL});
  const participant=await participantContext.newPage();
  try {
    await signInParticipant(participant);
    await participant.goto(sessionPath);
    await participant.locator('form[action$="/join"] button').press('Enter');
    await expect(participant.getByRole('status')).toContainText('Waiting for the creator');
    await page.reload();
    await page.locator('.webxdc-participant').filter({hasText:participantUsername(info)}).getByRole('button',{name:'Approve',exact:true}).press('Enter');
    await participant.reload();
    await expect(participant.locator('a.webxdc-open')).toBeVisible();
    if (scripted) {
      await page.locator('a.webxdc-open').press('Enter');
      await participant.locator('a.webxdc-open').press('Enter');
      await expect(page.locator('iframe.webxdc-frame')).toHaveAttribute('title','Accessible fixture session');
      const app=page.frameLocator('iframe.webxdc-frame');
      const peer=participant.frameLocator('iframe.webxdc-frame');
      await expect(app.getByRole('button',{name:'Send shared update'})).toBeVisible();
      await expect(peer.getByRole('status')).toHaveText('Waiting for an update');
      await app.getByRole('button',{name:'Send shared update'}).press('Enter');
      await expect(app.getByRole('status')).toHaveText('Shared update received');
      await expect(peer.getByRole('status')).toHaveText('Shared update received');
      await page.screenshot({path:info.outputPath('webxdc-player.png'),fullPage:true});
      const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
      expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
    }
    await page.goto(sessionPath);
    await page.locator('details.webxdc-details > summary').filter({hasText:'Manage session'}).press('Enter');
    await page.locator('form[action$="/close"] button').press('Enter');
    await expect(page.getByRole('status')).toContainText('This session has ended.');
    await participant.goto(sessionPath);
    await expect(participant.getByRole('status')).toContainText('This session has ended.');
  } finally { await participantContext.close(); }
}

test('Webxdc upload recovery, approval and shared app updates are accessible',async({page,browser},info)=>journey(page,browser,info));
test('@no-js Webxdc upload recovery, approval and closure remain native forms',async({page,browser},info)=>journey(page,browser,info));
