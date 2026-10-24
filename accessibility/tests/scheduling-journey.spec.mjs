import {test,expect} from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import {signIn} from '../session.mjs';

async function scheduleJourney(page,info) {
  await signIn(page);
  const text=`Scheduled fixture ${info.project.name}`;
  const tomorrow=new Date(Date.now()+24*3600*1000).toISOString().slice(0,16);
  const later=new Date(Date.now()+48*3600*1000).toISOString().slice(0,16);
  await page.goto('/compose');
  await page.locator('textarea[name=status]').fill(text);
  if (!await page.locator('[name=scheduled_at]').isVisible()) await page.locator('[data-compose-toggle=schedule]').press('Enter');
  await page.locator('[name=scheduled_at]').fill(tomorrow);
  await page.locator('button[name=op][value=post]').press('Enter');
  await page.waitForURL(url=>url.pathname==='/settings/scheduled');
  const row=page.locator('.list-index__item').filter({hasText:text});
  const form=row.locator('form[action$="/reschedule"]');
  await expect(form.locator('[name=scheduled_at]')).toHaveValue(tomorrow);
  await form.locator('[name=scheduled_at]').fill('2001-01-01T12:00');
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.locator('#form-error')).toBeVisible();
  await page.screenshot({path:info.outputPath('schedule-error.png'),fullPage:true});
  await expect.soft(page.locator('#form-error')).toBeFocused({timeout:1500});
  await expect.soft(form.locator('[name=scheduled_at]')).toHaveValue('2001-01-01T12:00',{timeout:1500});
  await expect.soft(form.locator('[name=scheduled_at]')).toHaveAttribute('aria-invalid','true',{timeout:1500});
  if (info.project.use.javaScriptEnabled!==false) {
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
    expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
  await form.locator('[name=scheduled_at]').fill(later);
  await form.locator('button[type=submit]').press('Enter');
  await expect(page.locator('.settings__saved')).toBeVisible();
  await expect(form.locator('[name=scheduled_at]')).toHaveValue(later);
  await page.reload();
  await expect(form.locator('[name=scheduled_at]')).toHaveValue(later);
  await row.locator('form[action$="/cancel"] button').press('Enter');
  await expect(row).toHaveCount(0);
}

test('scheduled post errors lead to rescheduling and cancellation',async({page},info)=>scheduleJourney(page,info));
test('@no-js scheduled post errors lead to rescheduling and cancellation',async({page},info)=>scheduleJourney(page,info));
