import {signInParticipant,participantUsername} from '../session.mjs';
import {test,expect} from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import {signIn,chooseInterfaceLanguage} from '../session.mjs';

async function conversationJourney(page,browser,info,russian) {
  if (info.project.use.javaScriptEnabled===false) await page.setViewportSize({width:320,height:800});
  await signIn(page);
  const username=`a11y_${info.project.name.replaceAll('-','_')}`;
  const recipientContext=await browser.newContext({...info.project.use,baseURL:process.env.A11Y_BASE_URL});
  const recipient=await recipientContext.newPage();
  try {
    if (russian) await chooseInterfaceLanguage(page,'ru');
    await signInParticipant(recipient);
    await page.goto(`/@${participantUsername(info)}`);
    const profile=page.locator('details.profile__menu');
    await profile.locator(':scope > summary').press('Enter');
    await profile.locator('a[href^="/compose?visibility=direct"]').press('Enter');
    await expect(page.locator('[name=visibility]')).toHaveValue('direct');
    const first=`Private fixture message ${info.project.name}`;
    await page.locator('textarea[name=status]').fill(`@${participantUsername(info)} ${first}`);
    await page.locator('button[name=op][value=post]').press('Enter');
    await page.waitForURL(url=>url.pathname.startsWith(`/@${username}/`));
    await recipient.goto('/conversations');
    const incoming=recipient.locator('.conversation').filter({hasText:first});
    await expect(incoming).toHaveClass(/is-unread/);
    await incoming.locator('.conversation__open').press('Enter');
    await expect(recipient.locator('form.compose-form [name=visibility]')).toHaveValue('direct');
    const reply=`Reply to fixture ${info.project.name}`;
    await recipient.locator('textarea[name=status]').fill(`@${username} ${reply}`);
    await recipient.locator('button[name=op][value=post]').press('Enter');
    await expect(recipient.locator('.status__content').filter({hasText:reply}).first()).toBeVisible();
    await page.goto('/conversations');
    const row=page.locator('.conversation').filter({hasText:reply});
    await expect(row).toHaveClass(/is-unread/);
    await row.locator('summary').press('Enter');
    await expect.soft(row.locator('summary')).toHaveAccessibleName(russian?'Действия с перепиской':'Conversation options',{timeout:1500});
    await expect.soft(row.locator('form[action$="/read"] button')).toHaveText(russian?'Отметить прочитанной':'Mark as read',{timeout:1500});
    await expect.soft(row.locator('form[data-confirm-action$="/remove"] button')).toHaveText(russian?'Убрать из входящих':'Remove from inbox',{timeout:1500});
    await page.screenshot({path:info.outputPath('conversation-menu.png'),fullPage:true});
    if (info.project.use.javaScriptEnabled!==false) {
      const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
      expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
    }
    if (page.viewportSize().width<=640) {
      const panel=await row.locator('.status__menu-pop').boundingBox();
      const link=await row.locator('.conversation__open').boundingBox();
      expect.soft(link.y).toBeGreaterThanOrEqual(panel.y+panel.height-1);
      // The two menu actions precede the still-visible conversation link.
      for (let i=0;i<3;i++) await page.keyboard.press('Tab');
      await expect(row.locator('.conversation__open')).toBeFocused();
    }
    await row.locator('form[action$="/read"] button').press('Enter');
    await expect(row).not.toHaveClass(/is-unread/);
    await row.locator('summary').press('Enter');
    await expect.soft(row.locator('form[action$="/unread"] button')).toHaveText(russian?'Отметить непрочитанной':'Mark as unread',{timeout:1500});
    await row.locator('form[action$="/unread"] button').press('Enter');
    await expect(row).toHaveClass(/is-unread/);
    await row.locator('summary').press('Enter');
    if (info.project.use.javaScriptEnabled!==false) page.once('dialog',dialog=>dialog.accept());
    await row.locator('form[data-confirm-action$="/remove"] button').press('Enter');
    if (info.project.use.javaScriptEnabled===false) await page.locator('.confirmation-card form[action$="/remove"] button').press('Enter');
    await expect(row).toHaveCount(0);
    // Removal hides the inbox row; a later message brings the thread back.
    const later=`Later fixture reply ${info.project.name}`;
    await recipient.locator('textarea[name=status]').fill(`@${username} ${later}`);
    await recipient.locator('button[name=op][value=post]').press('Enter');
    await expect(recipient.locator('.status__content').filter({hasText:later}).first()).toBeVisible();
    await page.reload();
    const returned=page.locator('.conversation').filter({hasText:later});
    await expect(returned).toHaveClass(/is-unread/);
    await returned.locator('.conversation__open').press('Enter');
    await expect(page.locator('.status__content').filter({hasText:later}).first()).toBeVisible();
  } finally {
    await recipientContext.close();
    if (russian) await chooseInterfaceLanguage(page,'en');
  }
}

test('Russian private mentions support reply, read state and inbox recovery',async({page,browser},info)=>conversationJourney(page,browser,info,true));
test('@no-js private mentions support reply, read state and native removal',async({page,browser},info)=>conversationJourney(page,browser,info,false));
