import {signInParticipant,participantUsername} from '../session.mjs';
import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {signIn} from '../session.mjs';

async function groupJourney(page,browser,info) {
  await signIn(page);
  await page.goto('/groups/new');
  const form=page.locator('form[action="/web/groups"]');
  const name=`grp_${info.project.name.replaceAll('-','_')}`;
  async function draft(handle) {
    await form.locator('[name=name]').fill(handle);
    await form.locator('[name=display_name]').fill('Accessibility group');
    await form.locator('[name=membership_policy]').selectOption('approval');
    await form.locator('[name=posting_policy]').selectOption('mods');
  }
  await draft('developer');
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.locator('#form-error')).toBeVisible();
  await page.screenshot({path:info.outputPath('group-create-error.png'),fullPage:true});
  await expect.soft(page.locator('#form-error')).toBeFocused({timeout:1500});
  await expect.soft(form.locator('[name=name]')).toHaveValue('developer',{timeout:1500});
  await expect.soft(form.locator('[name=name]')).toHaveAttribute('aria-invalid','true',{timeout:1500});
  await expect.soft(form.locator('[name=display_name]')).toHaveValue('Accessibility group',{timeout:1500});
  await expect.soft(form.locator('[name=membership_policy]')).toHaveValue('approval',{timeout:1500});
  await expect.soft(form.locator('[name=posting_policy]')).toHaveValue('mods',{timeout:1500});
  if (info.project.use.javaScriptEnabled!==false) {
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
    expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
  await draft(name);
  await form.locator('button[type=submit]').press('Enter');
  await expect(page).toHaveURL(url=>url.pathname===`/@${name}`);
  const postLink=page.locator('a.group-post-link');
  const groupId=new URL(await postLink.getAttribute('href'),page.url()).searchParams.get('group');
  await postLink.press('Enter');
  const text=`Group fixture post ${info.project.name}`;
  await page.locator('textarea[name=status]').fill(text);
  await page.locator('button[name=op][value=post]').press('Enter');
  await expect(page.locator('.status__content').filter({hasText:text}).first()).toBeVisible();
  const memberContext=await browser.newContext({...info.project.use,baseURL:process.env.A11Y_BASE_URL});
  const member=await memberContext.newPage();
  try {
    await signInParticipant(member);
    await member.goto(`/@${name}`);
    await member.locator('form[action$="/follow"] button').first().press('Enter');
    await expect(member.locator('form[action$="/unfollow"] button').first()).toHaveText('Cancel request');
    await page.goto(`/groups/${groupId}/requests`);
    const request=page.locator('.relationships-list__item').filter({hasText:participantUsername(info)});
    await request.locator('form[action$="/approve"] button').press('Enter');
    await expect(request).toHaveCount(0);
    await member.reload();
    await expect(member.locator('form[action$="/unfollow"] button').first()).toHaveText('Unfollow');
    await expect(member.locator('.status__content').filter({hasText:text}).first()).toBeVisible();
    await expect(member.locator('a.group-post-link')).toHaveCount(0);
    await member.locator('form[action$="/unfollow"] button').first().press('Enter');
    await expect(member.locator('form[action$="/follow"]')).toHaveCount(1);
  } finally { await memberContext.close(); }
}

test('group creation errors preserve choices through posting and join approval',async({page,browser},info)=>groupJourney(page,browser,info));
test('@no-js group creation errors preserve choices through posting and join approval',async({page,browser},info)=>groupJourney(page,browser,info));
