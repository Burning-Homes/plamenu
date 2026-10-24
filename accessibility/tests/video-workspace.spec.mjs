import {test, expect} from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import {signIn} from '../session.mjs';

async function audit(page, noScript) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  if (!noScript) {
    const result = await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
    expect(result.violations.map(v => ({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
}
async function journey(page, info, noScript) {
  test.setTimeout(120000);
  await signIn(page);
  await page.goto('/videos');
  await expect(page.getByRole('heading', {name:'Video studio',exact:true})).toBeVisible();
  const uploadLink = page.locator('.video-workspace__actions').getByRole('link',{name:'Upload a video',exact:true});
  const createLink = page.locator('.video-workspace__actions').getByRole('link',{name:'Create a video channel',exact:true});
  for (const link of [uploadLink,createLink]) {
    const box = await link.boundingBox();
    expect(box.height).toBeGreaterThanOrEqual(44);
    expect(box.y+box.height).toBeLessThan(await page.evaluate(()=>innerHeight));
  }
  const more=page.locator('.video-workspace__more > summary');
  await more.press('Enter');
  await expect(page.locator('.video-workspace__more').getByRole('link',{name:'Import a video',exact:true})).toBeVisible();
  await audit(page,noScript);
  await more.press('Enter');
  await page.getByRole('searchbox',{name:'Search your videos'}).fill('Catalogue boundary visible fixture');
  await page.getByRole('button',{name:'Search',exact:true}).press('Enter');
  const rows = page.locator('.video-workspace__row');
  await expect(rows).toHaveCount(20);
  await expect(page.getByRole('link',{name:'Manage captions',exact:true})).toHaveCount(0);
  const first = await rows.first().locator('.video-workspace__title').getAttribute('href');
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('video-studio.png'),fullPage:true});
  await page.getByRole('link',{name:'Show older items',exact:true}).press('Enter');
  await expect(rows).toHaveCount(20);
  expect(await rows.locator('.video-workspace__title').evaluateAll(nodes=>nodes.map(n=>n.getAttribute('href')))).not.toContain(first);
  await page.getByRole('link',{name:'Show older items',exact:true}).press('Enter');
  await expect(rows).toHaveCount(10);
  await expect(page.getByRole('link',{name:'Show older items',exact:true})).toHaveCount(0);
  await rows.first().locator('.video-workspace__title').press('Enter');
  await expect(page.locator('.video-workspace__tools > a')).toHaveCount(5);
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('video-management.png'),fullPage:true});
  await page.goto('/videos');
  await createLink.press('Enter');
  const name = `ux_${Date.now().toString(36)}_${info.project.name.replaceAll('-','').slice(0,8)}`;
  const form = page.locator('form[action="/web/video-channels"]');
  await form.getByLabel('Channel address').fill(name);
  await form.getByLabel('Channel name').fill(`UX ${name}`);
  await audit(page,noScript);
  await form.getByRole('button',{name:'Create a video channel',exact:true}).press('Enter');
  await page.waitForURL(url=>/^\/videos\/channels\/\d+$/.test(url.pathname));
  await expect(page.getByRole('heading',{level:1})).toContainText(`UX ${name}`);
  await page.goto('/videos/channels/new');
  await form.getByLabel('Channel address').fill(name);
  await form.getByLabel('Channel name').fill('Keep this entered name');
  await form.getByRole('button',{name:'Create a video channel',exact:true}).press('Enter');
  await expect(page.getByRole('alert')).toBeVisible();
  await expect(form.getByLabel('Channel address')).toHaveValue(name);
  await expect(form.getByLabel('Channel name')).toHaveValue('Keep this entered name');
  await page.goto('/videos');
  await uploadLink.press('Enter');
  await expect(page.locator('form[action="/web/videos"] input[name="file"]')).toBeVisible();
  const options=page.locator('[data-video-options]');
  await expect(options).not.toHaveAttribute('open','');
  await options.locator(':scope > summary').press('Enter');
  await expect(options.getByLabel('Description',{exact:true})).toBeVisible();
  await options.locator(':scope > summary').press('Enter');
  await expect(page.locator('.video-workspace__row')).toHaveCount(0);
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('video-upload.png'),fullPage:true});
  await page.goto('/videos/playlists?mine=true');
  const create = page.locator('.video-workspace__create');
  if (await create.getAttribute('open') === null) await create.locator('summary').press('Enter');
  await expect(create.locator('form').first()).toBeVisible();
  await audit(page,noScript);
  await page.goto('/videos/sources');
  await expect(page.locator('#source-create')).toBeVisible();
  const source=page.locator('.video-workspace__source');
  await expect(source).toHaveCount(1);
  expect(await page.locator('#source-create').evaluate(e=>e.getBoundingClientRect().top)).toBeLessThan(await source.evaluate(e=>e.getBoundingClientRect().top));
  await source.locator('summary').filter({hasText:'Schedule and check details'}).press('Enter');
  await source.locator('summary').filter({hasText:'Remove source'}).press('Enter');
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('scheduled-sources.png'),fullPage:true});
}
test('Creator workspace puts creation first and pages its library',async({page},info)=>journey(page,info,false));
test('Creator workspace authoring and paging work without scripts @no-js',async({page},info)=>journey(page,info,true));
