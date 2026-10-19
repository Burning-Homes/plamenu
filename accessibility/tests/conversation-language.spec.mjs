import {signInParticipant,participantUsername} from '../session.mjs';
import {test,expect} from '@playwright/test';
import {signIn,chooseVideoLanguage} from '../session.mjs';

async function declaredLanguageJourney(page,browser,info) {
  await signIn(page);
  const guestContext=await browser.newContext({...info.project.use,baseURL:process.env.A11Y_BASE_URL});
  const guest=await guestContext.newPage();
  try {
    await signInParticipant(guest);
    for (const warning of [false,true]) {
      await page.goto('/compose?visibility=direct');
      const form=page.locator('form.compose-form');
      const text=`Bonjour, voici notre message ${info.project.name} ${warning?'avec avertissement':'sans avertissement'}.`;
      const summary=`Discussion en français ${info.project.name}`;
      await form.locator('textarea[name=status]').fill(`@${participantUsername(info)} ${text}`);
      await chooseVideoLanguage(form,'fr');
      if (warning) {
        if (info.project.use.javaScriptEnabled!==false) await form.locator('[data-compose-toggle=cw]').press('Enter');
        await form.locator('[name=spoiler_text]').fill(summary);
      }
      await form.locator('button[name=op][value=post]').press('Enter');
      await expect(page).toHaveURL(url=>url.pathname.startsWith('/@'));
      await guest.goto('/conversations');
      const row=guest.locator('.conversation').filter({hasText:warning?summary:text});
      await expect(row).toHaveCount(1);
      await expect.soft(row.locator('.conversation__preview')).toHaveAttribute('lang','fr',{timeout:1500});
      await expect(guest.locator('html')).toHaveAttribute('lang','en-US');
      await expect(row.locator('.conversation__meta')).toContainText('Open conversation');
      expect(await row.locator('.conversation__meta').evaluate(el=>el.closest('[lang]').lang)).toBe('en-US');
      await row.locator('.conversation__open').press('Enter');
      const content=guest.locator('.status__content').filter({hasText:text}).first();
      await expect(content).toHaveAttribute('lang','fr');
      if (warning) await guest.locator('details.status__cw').first().locator(':scope > summary').press('Enter');
      await expect(content).toBeVisible();
    }
  } finally {await guestContext.close();}
}

test('private-message previews preserve declared French text and warning language',async({page,browser},info)=>declaredLanguageJourney(page,browser,info));
test('@no-js private-message previews preserve declared language',async({page,browser},info)=>declaredLanguageJourney(page,browser,info));
