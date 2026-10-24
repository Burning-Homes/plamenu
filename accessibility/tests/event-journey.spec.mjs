import {signInParticipant} from '../session.mjs';
import {test,expect} from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import {signIn} from '../session.mjs';

async function audit(page,info,name) {
  if (name) await page.screenshot({path:info.outputPath(`${name}.png`),fullPage:true});
  if (info.project.use.javaScriptEnabled===false) return;
  const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
  expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
}

async function eventJourney(page,browser,info) {
  const scripted=info.project.use.javaScriptEnabled!==false;
  await signIn(page);
  await page.goto('/compose?kind=event');
  const form=page.locator('form.compose-form');
  const title=`Event fixture ${info.project.name}`;
  await form.locator('[name=title]').fill(title);
  await form.locator('textarea[name=status]').fill('Bring your questions to this disposable fixture event.');
  await form.locator('[name=event_start]').fill('2030-01-15T18:30');
  await form.locator('[name=event_end]').fill('2030-01-15T17:00');
  await form.locator('[name=event_timezone]').selectOption('Europe/Berlin');
  await form.locator('[name=event_location]').fill('Fixture community room');
  await form.locator('[name=event_capacity]').fill('10');
  await form.locator('button[name=op][value=post]').press('Enter');
  const error=page.locator('p.compose__error[role=alert]:visible');
  await expect(error).toBeVisible();
  await audit(page,info,'event-compose-error');
  await expect.soft(error).toBeFocused({timeout:1500});
  if (scripted) {
    await page.emulateMedia({colorScheme:'dark'});
    // Theme changes animate control colours; audit their settled rendered state.
    await page.evaluate(async()=>{
      await Promise.all(document.getAnimations()
        .filter(animation=>animation.effect?.getTiming().iterations!==Infinity)
        .map(animation=>animation.finished.catch(()=>{})));
    });
    await audit(page,info,null);
    await page.emulateMedia({colorScheme:'light'});
  }
  await expect(form.locator('[name=title]')).toHaveValue(title);
  await expect(form.locator('[name=event_end]')).toHaveValue('2030-01-15T17:00');
  await expect(form.locator('[name=event_timezone]')).toHaveValue('Europe/Berlin');
  await expect(form.locator('[name=event_location]')).toHaveValue('Fixture community room');
  await form.locator('[name=event_end]').fill('2030-01-15T20:00');
  await form.locator('button[name=op][value=post]').press('Enter');
  await expect(page.locator('.status__event')).toBeVisible();
  const permalink=new URL(page.url()).pathname;
  const guestContext=await browser.newContext({...info.project.use,baseURL:process.env.A11Y_BASE_URL});
  const guest=await guestContext.newPage();
  try {
    await signInParticipant(guest);
    await guest.goto(permalink);
    const region=guest.locator('[data-rsvp]').first();
    const attend=region.locator('form[action$="/participate"] button');
    const withdraw=region.locator('form[action$="/unparticipate"] button');
    if (scripted) {
      await guest.route('**/web/statuses/*/participate',route=>route.fulfill({status:503,body:'Fixture refusal'}));
      await attend.press('Enter');
      await expect(region.getByRole('alert')).toBeVisible();
      await expect(attend).toBeEnabled();
      await audit(guest,info,'event-rsvp-failure');
      await expect.soft(attend).toBeFocused({timeout:1500});
      await guest.unroute('**/web/statuses/*/participate');
    }
    await attend.press('Enter');
    await expect(region.locator('.status__event-going')).toBeVisible();
    await audit(guest,info,'event-rsvp-going');
    await expect.soft(region.locator('.status__event-going')).toHaveAttribute('role','status',{timeout:1500});
    if (scripted) {
      await expect.soft(withdraw).toBeFocused({timeout:1500});
      let release;
      const gate=new Promise(resolve=>{release=resolve;});
      await guest.route('**/web/statuses/*/unparticipate',async route=>{await gate;await route.fulfill({status:503,body:'Fixture delayed refusal'});});
      await withdraw.press('Enter');
      await expect(withdraw).toBeDisabled();
      const other=guest.locator('textarea[name=status]');
      await other.focus();
      release();
      await expect(region.getByRole('alert')).toBeVisible();
      await expect(withdraw).toBeEnabled();
      await expect(other).toBeFocused();
      await guest.unroute('**/web/statuses/*/unparticipate');
    }
    await withdraw.press('Enter');
    await expect(attend).toBeVisible();
    if (scripted) await expect.soft(attend).toBeFocused({timeout:1500});
    await attend.press('Enter');
    await expect(region.locator('.status__event-going')).toBeVisible();
    await page.locator('form[action$="/cancel-event"] button').press('Enter');
    await expect(page.locator('.status__event-cancelled').first()).toBeVisible();
    await guest.reload();
    await expect(guest.locator('.status__event-cancelled').first()).toBeVisible();
    await expect(region.locator('form[action$="/participate"]')).toHaveCount(0);
  } finally { await guestContext.close(); }
}

test('event correction, RSVP failure recovery and cancellation preserve keyboard context',async({page,browser},info)=>eventJourney(page,browser,info));
test('@no-js event correction, attendance and cancellation remain native forms',async({page,browser},info)=>eventJourney(page,browser,info));
