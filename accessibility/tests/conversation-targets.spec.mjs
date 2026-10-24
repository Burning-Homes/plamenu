import {test,expect} from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import {signIn,signInParticipant,participantUsername,chooseInterfaceLanguage} from '../session.mjs';

async function targetJourney(page,browser,info,multiple=false) {
  const scripted=info.project.use.javaScriptEnabled!==false;
  if(!scripted) await page.setViewportSize({width:320,height:800});
  await signIn(page);
  const owner=`a11y_${info.project.name.replaceAll('-','_')}`;
  const peer=participantUsername(info);
  const longName=`participant_${info.project.name.replaceAll('-','_')}`.slice(0,30);
  const guestContext=await browser.newContext({...info.project.use,baseURL:process.env.A11Y_BASE_URL});
  const guest=await guestContext.newPage();
  let originalName;
  try {
    if(scripted) await chooseInterfaceLanguage(page,'ru');
    await signInParticipant(guest);
    await guest.goto('/settings/profile');
    const profile=guest.locator('form[action="/web/settings/profile"]');
    originalName=await profile.locator('[name=display_name]').inputValue();
    await profile.locator('[name=display_name]').fill(longName);
    await profile.locator('button[type=submit]').press('Enter');
    await expect(guest.locator('.settings__saved')).toContainText('Profile saved');
    await page.goto('/compose?visibility=direct');
    const message=`Long-name ${multiple?"group":"single"} fixture ${info.project.name}`;
    await page.locator('textarea[name=status]').fill(`@${peer} ${multiple?"@channel_editor ":""}${message}`);
    await page.locator('button[name=op][value=post]').press('Enter');
    await expect(page).toHaveURL(url=>url.pathname.startsWith(`/@${owner}/`));
    await guest.goto('/conversations');
    await guest.locator('.conversation').filter({hasText:message}).locator('.conversation__open').press('Enter');
    const reply=`Name target ${multiple?"group":"single"} reply ${info.project.name}`;
    await guest.locator('textarea[name=status]').fill(`@${owner} ${multiple?"@channel_editor ":""}${reply}`);
    await guest.locator('button[name=op][value=post]').press('Enter');
    await expect(guest.locator('.status__content').filter({hasText:reply}).first()).toBeVisible();
    await page.goto('/conversations');
    const row=page.locator('.conversation').filter({hasText:reply});
    await row.locator('summary').press('Enter');
    if(scripted) {
      const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
      expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
    }
    const names=row.locator('.conversation__participant');
    await expect(names).toHaveCount(multiple?2:1);
    const name=names.filter({hasText:longName});
    await expect(name).toHaveAccessibleName(longName);
    await name.focus();
    await expect(name).toBeFocused();
    await row.screenshot({path:info.outputPath(multiple?'conversation-participant-group.png':'conversation-participant-focus.png')});
    const geometry=await name.evaluate(link=>{
      const box=link.getBoundingClientRect(),clip=link.closest('.conversation__with').getBoundingClientRect();
      const left=Math.max(box.left,clip.left),right=Math.min(box.right,clip.right);
      const top=Math.max(box.top,clip.top),bottom=Math.min(box.bottom,clip.bottom);
      const style=getComputedStyle(link);
      const outline=Math.max(0,parseFloat(style.outlineWidth)+parseFloat(style.outlineOffset));
      return {width:right-left,height:bottom-top,
        outlineFits:box.top-clip.top>=outline-0.1&&clip.bottom-box.bottom>=outline-0.1,
        hit:document.elementFromPoint((left+right)/2,(top+bottom)/2)?.closest('a')===link};
    });
    expect.soft(geometry.width).toBeGreaterThanOrEqual(24);
    expect.soft(geometry.height).toBeGreaterThanOrEqual(24);
    expect(geometry.hit).toBe(true);
    expect.soft(geometry.outlineFits).toBe(true);
    if(multiple) {
      const other=names.filter({hasText:'channel_editor'});
      const size=await other.boundingBox();
      expect.soft(size.width).toBeGreaterThanOrEqual(24);
      expect.soft(size.height).toBeGreaterThanOrEqual(24);
      const reachable=await other.evaluate(link=>{
        const box=link.getBoundingClientRect();
        return document.elementFromPoint(box.x+box.width/2,box.y+box.height/2)?.closest('a')===link;
      });
      expect.soft(reachable).toBe(true);
    }
    await name.press('Enter');
    await expect(page).toHaveURL(url=>url.pathname===`/@${peer}`);
  } finally {
    if(originalName!==undefined) {
      await guest.goto('/settings/profile');
      const profile=guest.locator('form[action="/web/settings/profile"]');
      await profile.locator('[name=display_name]').fill(originalName);
      await profile.locator('button[type=submit]').press('Enter');
      await expect(guest.locator('.settings__saved')).toContainText('Profile saved');
    }
    await guestContext.close();
    if(scripted) await chooseInterfaceLanguage(page,'en');
  }
}

test('long inbox participant names retain usable targets and keyboard navigation',async({page,browser},info)=>targetJourney(page,browser,info));
test('@no-js long inbox participant names retain usable targets',async({page,browser},info)=>targetJourney(page,browser,info));

test('multiple inbox participants share visible targets without hiding a long name',async({page,browser},info)=>targetJourney(page,browser,info,true));
test('@no-js multiple inbox participants retain visible targets',async({page,browser},info)=>targetJourney(page,browser,info,true));
