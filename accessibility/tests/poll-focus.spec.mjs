import {signInParticipant} from '../session.mjs';
import {test,expect} from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import {signIn} from '../session.mjs';

async function audit(page,info,name) {
  await page.screenshot({path:info.outputPath(`${name}.png`),fullPage:true});
  if (info.project.use.javaScriptEnabled!==false) {
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
    expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
}

async function pollFocusJourney(page,browser,info) {
  const scripted=info.project.use.javaScriptEnabled!==false;
  await signIn(page);
  await page.goto('/compose');
  const form=page.locator('form.compose-form');
  await form.locator('textarea[name=status]').fill(`When should we meet? ${info.project.name}`);
  if (scripted) await form.locator('[data-compose-toggle=poll]').press('Enter');
  await form.locator('[name="poll_options[]"]').nth(0).fill('Morning');
  await form.locator('[name="poll_options[]"]').nth(1).fill('Evening');
  await form.locator('button[name=op][value=post]').press('Enter');
  await expect(page).toHaveURL(url=>url.pathname.startsWith('/@'));
  const permalink=new URL(page.url()).pathname;
  const guestContext=await browser.newContext({...info.project.use,baseURL:process.env.A11Y_BASE_URL});
  const guest=await guestContext.newPage();
  try {
    await signInParticipant(guest);
    await guest.goto(permalink);
    const region=guest.locator('[data-poll]').first();
    const vote=region.locator('button[type=submit]');
    await region.getByLabel('Morning',{exact:true}).check();
    if (scripted) {
      await guest.route('**/web/statuses/*/vote',route=>route.fulfill({status:503,body:'Fixture refusal'}));
      await vote.press('Enter');
      await expect(region.getByRole('alert')).toBeVisible();
      await expect(vote).toBeEnabled();
      await audit(guest,info,'poll-vote-failure');
      await expect.soft(vote).toBeFocused({timeout:1500});
      await expect(region.getByLabel('Morning',{exact:true})).toBeChecked();
      await guest.unroute('**/web/statuses/*/vote');
      let release;
      const gate=new Promise(resolve=>{release=resolve;});
      await guest.route('**/web/statuses/*/vote',async route=>{await gate;await route.abort('failed');});
      await vote.press('Enter');
      await expect(vote).toBeDisabled();
      const other=guest.locator('textarea[name=status]');
      await other.focus();
      release();
      await expect(region.getByRole('alert')).toBeVisible();
      await expect(vote).toBeEnabled();
      await expect(other).toBeFocused();
      await guest.unroute('**/web/statuses/*/vote');
    }
    await vote.press('Enter');
    await expect(region.locator('.poll__result.is-own')).toContainText('Morning');
    await expect(region.locator('.poll__footer')).toHaveText(/^\u2068?1\u2069? vote$/);
    await audit(guest,info,'poll-vote-results');
    if (scripted) {
      await expect.soft(region).toBeFocused({timeout:1500});
      await expect.soft(region).toHaveAttribute('tabindex','-1',{timeout:1500});
      await guest.keyboard.press('Tab');
      expect(await region.evaluate(el=>el.contains(document.activeElement))).toBe(false);
    }
    await guest.reload();
    await expect(region.locator('.poll__result.is-own')).toContainText('Morning');
    await expect(region.locator('.poll__footer')).toHaveText(/^\u2068?1\u2069? vote$/);
    if (scripted) {
      // A successful response must also leave later navigation alone.
      await page.goto('/compose');
      await form.locator('textarea[name=status]').fill(`Another meeting poll ${info.project.name}`);
      await form.locator('[data-compose-toggle=poll]').press('Enter');
      await form.locator('[name="poll_options[]"]').nth(0).fill('Monday');
      await form.locator('[name="poll_options[]"]').nth(1).fill('Tuesday');
      await form.locator('button[name=op][value=post]').press('Enter');
      await expect(page).toHaveURL(url=>url.pathname.startsWith('/@'));
      await guest.goto(new URL(page.url()).pathname);
      await region.getByLabel('Monday',{exact:true}).check();
      let release;
      const gate=new Promise(resolve=>{release=resolve;});
      await guest.route('**/web/statuses/*/vote',async route=>{await gate;await route.continue();});
      await vote.press('Enter');
      await expect(vote).toBeDisabled();
      const other=guest.locator('textarea[name=status]');
      await other.focus();
      release();
      await expect(region.locator('.poll__result.is-own')).toContainText('Monday');
      await expect(other).toBeFocused();
    }
  } finally {await guestContext.close();}
}

test('poll voting preserves keyboard context after refusal and results replacement',async({page,browser},info)=>pollFocusJourney(page,browser,info));
test('@no-js poll voting retains the native form journey',async({page,browser},info)=>pollFocusJourney(page,browser,info));
