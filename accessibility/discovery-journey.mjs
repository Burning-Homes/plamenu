// Runs only against a disposable Rust test server; no standing instance changes.
import { chromium, expect } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import assert from 'node:assert/strict';
let input = '';
for await (const chunk of process.stdin) input += chunk;
const config = JSON.parse(input);
const effects = async () => (await fetch(`${config.url}/_fixture/discovery-effects`)).json();
const before = await effects();
const browser = await chromium.launch({ headless: true });
const session = async options => {
  const context = await browser.newContext(options);
  // Install a real host-only cookie so redirects preserve the session. Chromium
  // permits Secure cookies on trusted loopback HTTP for this disposable server.
  await context.addCookies([{ name: '__Host-plamenu_session', value: config.token,
    url: `https://${new URL(config.url).hostname}/`, secure: true, httpOnly: true, sameSite: 'Lax' }]);
  context.setDefaultTimeout(10000);
  context.setDefaultNavigationTimeout(10000);
  return context;
};
try {
  for (const [name, width, height] of [['desktop', 1280, 900], ['mobile', 390, 844], ['narrow', 320, 640], ['dark', 1280, 900], ['no-js-mobile', 390, 844], ['no-js-desktop', 1280, 900]]) {
    const context = await session({ viewport: { width, height }, colorScheme: name === 'dark' ? 'dark' : 'light', javaScriptEnabled: !name.startsWith('no-js') });
    context.setDefaultTimeout(10000);
    context.setDefaultNavigationTimeout(10000);
    // Exercise the native manual fallback here; real observers run below.
    await context.addInitScript(() => { delete window.IntersectionObserver; });
    const remote = [];
    await context.route('**/*', route => {
      if (new URL(route.request().url()).origin !== config.url) { remote.push(route.request().url()); return route.abort(); }
      return route.continue();
    });
    const page = await context.newPage();
    await page.goto(`${config.url}/discover?seen=true`);
    await page.getByRole('heading', { name: 'Discover', exact: true }).waitFor();
    assert.equal(await page.locator('[data-discovery-card]').count(), 20);
    const tabRows=await page.locator('.discovery-tabs a').evaluateAll(nodes=>nodes.map(node=>Math.round(node.getBoundingClientRect().top)));
    assert.equal(tabRows.length,6);
    assert.ok(tabRows[0]===tabRows[1] && tabRows[1]===tabRows[2] && tabRows[3]===tabRows[4] && tabRows[4]===tabRows[5] && tabRows[0]<tabRows[3], `${name}: balanced discovery navigation`);
    await page.getByRole('searchbox', {name:'Search',exact:true}).fill('alice');
    await page.getByRole('button', {name:'Apply',exact:true}).click();
    await expect(page.locator('[data-discovery-card]')).toHaveCount(1);
    await page.locator('details.discovery-wider > summary').click();
    await expect(page.getByRole('group', {name:'Send this query to:'})).toBeVisible();
    await expect(page.getByRole('checkbox', {name:'one.example',exact:true})).toBeChecked();
    await expect(page.getByRole('checkbox', {name:'two.example',exact:true})).toBeChecked();
    assert.ok(await page.locator('body').evaluate(el=>el.scrollWidth<=innerWidth), `${name}: wider search overflow`);
    if (!name.startsWith('no-js')) {
      const issues=(await new AxeBuilder({page}).include('main').withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze()).violations;
      assert.deepEqual(issues.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)})),[],`${name}: wider search accessibility`);
    }
    await page.screenshot({path:`/tmp/plamenu-discovery-step10-${name}.png`,fullPage:true});
    await page.goto(`${config.url}/discover?seen=true`);
    assert.ok(await page.locator('body').evaluate(el => el.scrollWidth <= innerWidth), `${name}: horizontal overflow`);
    assert.equal(await page.locator('[data-discovery-card] img, [data-discovery-card] video, [data-discovery-card] audio').count(), 0);
    // axe uses page timers; disabled-script contexts exercise the real interaction path separately.
    const violations = name.startsWith('no-js') ? [] : (await new AxeBuilder({ page }).include('main').withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze()).violations;
    assert.deepEqual(violations.map(v => ({ id: v.id, nodes: v.nodes.map(n => n.target) })), [], `${name}: accessibility violations`);
    await page.screenshot({ path: `/tmp/plamenu-discovery-step5-${name}.png`, fullPage: true });
    let reached = false;
    for (let n = 0; n < 90; n++) {
      await page.keyboard.press('Tab');
      if (await page.evaluate(() => document.activeElement?.textContent ?? '') === 'Open profile') { reached = true; break; }
    }
    assert.ok(reached, `${name}: Open profile reachable by keyboard`);
    const firstIds = await page.locator('[data-discovery-card]').evaluateAll(nodes => nodes.map(node => node.id));
    await page.getByRole('link', { name: 'Next page', exact: true }).click();
    if (name.startsWith('no-js')) {
      assert.equal(await page.locator('[data-discovery-card]').count(), 5);
      const nextIds = await page.locator('[data-discovery-card]').evaluateAll(nodes => nodes.map(node => node.id));
      assert.ok(nextIds.every(id => !firstIds.includes(id)), 'continuation does not repeat the first page');
    } else {
      await expect(page.locator('[data-discovery-card]')).toHaveCount(25);
      assert.equal(new URL(page.url()).search, '?seen=true', 'append without navigation');
      assert.equal(await page.evaluate(() => document.activeElement?.tagName), 'H2', 'manual loading focuses the new results');
      const appendedIds = await page.locator('[data-discovery-card]').evaluateAll(nodes => nodes.map(node => node.id));
      assert.deepEqual(appendedIds.slice(0, 20), firstIds, 'append preserves the initial page');
      assert.equal(new Set(appendedIds).size, 25, 'append contains every profile exactly once');
      assert.equal(await page.locator('.discovery-more').count(), 1);
      assert.equal(await page.locator('.discovery-more').isVisible(), false);
    }
    await page.goto(`${config.url}/discover?sources=true`);
    assert.equal(await page.locator('[data-discovery-server]').count(), 2);
    assert.equal(await page.getByText('Public directory verified', { exact: true }).count(), 0);
    assert.ok(await page.locator('body').evaluate(el => el.scrollWidth <= innerWidth), `${name}: server cards overflow`);
    if (!name.startsWith('no-js')) {
      const serverViolations = (await new AxeBuilder({ page }).include('main').withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze()).violations;
      assert.deepEqual(serverViolations.map(v => ({ id: v.id, nodes: v.nodes.map(n => n.target) })), [], `${name}: server accessibility violations`);
    }
    await page.goto(`${config.url}/discover/posts?seen=true`);
    await page.getByRole('searchbox', { name: 'Search', exact: true }).fill('orchids');
    await page.getByRole('button', { name: 'Apply', exact: true }).click();
    const initialPosts = await page.locator('[data-discovery-post]').count();
    assert.ok(initialPosts === 20, `${name}: local post results and bounded first page`);
    assert.equal(await page.locator('.discovery-post__warning[open]').count(), 0, 'content warnings start closed');
    await page.locator('.discovery-post__warning summary').first().click();
    assert.ok(await page.locator('body').evaluate(el => el.scrollWidth <= innerWidth), `${name}: post cards overflow`);
    if (!name.startsWith('no-js')) {
      const postViolations = (await new AxeBuilder({ page }).include('main').withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze()).violations;
      assert.deepEqual(postViolations.map(v => ({ id: v.id, nodes: v.nodes.map(n => n.target) })), [], `${name}: post accessibility violations`);
    }
    await page.screenshot({ path: `/tmp/plamenu-discovery-step7-${name}.png`, fullPage: true });
    await page.getByRole('link', { name: 'Next page', exact: true }).click();
    await expect(page.locator('[data-discovery-post]')).toHaveCount(name.startsWith('no-js') ? 27 - initialPosts : 27);
    assert.equal(new URL(page.url()).searchParams.get('q'), 'orchids', 'pagination preserves the query');
    await page.goto(`${config.url}/discover/communities?seen=true`);
    const initialCommunities = await page.locator('[data-discovery-card]').count();
    assert.ok(initialCommunities === 20, `${name}: bounded community page`);
    await expect(page.getByRole('button', {name: 'Open community', exact:true}).first()).toBeVisible();
    assert.ok(await page.locator('body').evaluate(el => el.scrollWidth <= innerWidth), `${name}: community cards overflow`);
    await page.screenshot({path: `/tmp/plamenu-discovery-step8-${name}.png`, fullPage:true});
    if (!name.startsWith('no-js')) {
      const issues = (await new AxeBuilder({page}).include('main').withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze()).violations;
      assert.deepEqual(issues.map(v => ({id:v.id,nodes:v.nodes.map(n=>n.target)})), [], `${name}: community accessibility violations`);
    }
    await page.getByRole('link', {name: 'Next page', exact:true}).click();
    await expect(page.locator('[data-discovery-card]')).toHaveCount(name.startsWith('no-js') ? 25-initialCommunities : 25);
    await page.goto(`${config.url}/discover/videos?seen=true`);
    await page.getByRole('searchbox',{name:'Search',exact:true}).fill('orchids');
    await page.getByRole('button',{name:'Apply',exact:true}).click();
    const initialVideos=await page.locator('[data-discovery-video]').count();
    assert.ok(initialVideos===20,`${name}: bounded video page`);
    await expect(page.getByRole('button',{name:'Open video',exact:true}).first()).toBeVisible();
    assert.equal(await page.locator('[data-discovery-video] img, [data-discovery-video] video').count(),0);
    assert.equal(await page.locator('.discovery-post__warning[open]').count(),0);
    await page.locator('.discovery-post__warning summary').first().click();
    assert.ok(await page.locator('body').evaluate(el=>el.scrollWidth<=innerWidth),`${name}: video overflow`);
    if (!name.startsWith('no-js')) {
      const issues=(await new AxeBuilder({page}).include('main').withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze()).violations;
      assert.deepEqual(issues.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)})),[],`${name}: video accessibility`);
    }
    await page.screenshot({path:`/tmp/plamenu-discovery-video-${name}.png`,fullPage:true});
    await page.getByRole('link',{name:'Next page',exact:true}).click();
    await expect(page.locator('[data-discovery-video]')).toHaveCount(name.startsWith('no-js')?25-initialVideos:25);
    assert.equal(new URL(page.url()).searchParams.get('q'),'orchids');
    await page.getByRole('link',{name:'Garden films',exact:true}).first().click();
    await expect(page.getByRole('heading',{name:'Garden films',exact:true})).toBeVisible();
    await expect(page.getByRole('button',{name:'Open channel',exact:true})).toBeVisible();
    assert.ok(await page.locator('[data-discovery-video]').count()>0);
    assert.ok(await page.locator('body').evaluate(el=>el.scrollWidth<=innerWidth),`${name}: channel overflow`);
    await page.goto(`${config.url}/discover/topics`);
    await page.getByRole('link', {name:'#garden',exact:true}).click();
    await expect(page.getByRole('heading', {name:'#garden',exact:true})).toBeVisible();
    const initialTopics = await page.locator('[data-discovery-results="topics"] > article').count();
    assert.ok(initialTopics > 0 && initialTopics < 102, `${name}: bounded mixed topic page`);
    await page.getByRole('button', {name:'Save topic',exact:true}).click();
    await expect(page.getByRole('button', {name:'Remove saved topic',exact:true})).toBeVisible();
    assert.ok(await page.locator('body').evaluate(el => el.scrollWidth <= innerWidth), `${name}: topic overflow`);
    if (!name.startsWith('no-js')) {
      const issues = (await new AxeBuilder({page}).include('main').withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze()).violations;
      assert.deepEqual(issues.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)})),[],`${name}: topic accessibility`);
    }
    await page.screenshot({path:`/tmp/plamenu-discovery-step9-${name}.png`,fullPage:true});
    await page.getByRole('link', {name:'Next page',exact:true}).click();
    if (name.startsWith('no-js')) {
      assert.ok(new URL(page.url()).searchParams.has('watermark'));
    } else {
      await expect.poll(()=>page.locator('[data-discovery-results="topics"] > article').count()).toBeGreaterThan(initialTopics);
    }
    assert.equal(new URL(page.url()).searchParams.get('topic'),'garden');
    await page.getByRole('link',{name:'Explore connections',exact:true}).first().click();
    await expect(page.getByRole('heading',{name:'Tagged topics',exact:true})).toBeVisible();
    assert.ok(await page.locator('body').evaluate(el => el.scrollWidth <= innerWidth), `${name}: connections overflow`);
    if (!name.startsWith('no-js')) {
      const issues=(await new AxeBuilder({page}).include('main').withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze()).violations;
      assert.deepEqual(issues.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)})),[],`${name}: connections accessibility`);
    }
    await page.goto(`${config.url}/discover/topics`);
    await page.getByRole('button',{name:'Clear saved topics',exact:true}).click();
    await expect(page.getByText('Save a topic to return to it here.',{exact:true})).toBeVisible();
    await page.goto(`${config.url}/admin/discovery`);
    await page.getByRole('heading', { name: 'Discovery policy', exact: true }).waitFor();
    await expect(page.getByRole('heading', {name:'Acquisition cost',exact:true})).toBeVisible();
    await expect(page.getByRole('region', {name:'Acquisition cost',exact:true}).locator('tbody tr')).toHaveCount(3);
    assert.ok(await page.locator('body').evaluate(el => el.scrollWidth <= innerWidth), `${name}: admin horizontal overflow`);
    await page.getByText('Enabled mechanisms', { exact: true }).click();
    await expect(page.locator('input[name=directory]')).toBeVisible();
    await expect(page.locator('input[name=community_directory]')).toBeVisible();
    await expect(page.locator('input[name=video_catalogue]')).toBeVisible();
    await expect(page.locator('input[name=video_indexing]')).toBeVisible();
    await page.getByText('Starter servers, import & export', { exact: true }).click();
    await expect(page.getByLabel('Source list JSON', { exact: true })).toBeVisible();
    assert.ok(await page.locator('body').evaluate(el => el.scrollWidth <= innerWidth), `${name}: expanded admin horizontal overflow`);
    if (!name.startsWith('no-js')) {
      const adminViolations = (await new AxeBuilder({ page }).include('main').withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze()).violations;
      assert.deepEqual(adminViolations.map(v => ({ id: v.id, nodes: v.nodes.map(n => n.target) })), [], `${name}: operator accessibility violations`);
    }
    if (name === 'desktop') await page.screenshot({ path: '/tmp/plamenu-discovery-step5-operator.png', fullPage: true });
    assert.deepEqual(remote, [], 'browsing must not load remote resources');
    await context.close();
    console.log(`PASS ${name}: cards, paging, sources, operator view and keyboard access`);
  }
  for (const fail of [false, true]) {
    const context = await session({ viewport: { width: 390, height: 844 } });
    const page = await context.newPage();
    let requests = 0;
    await page.route('**/discover?**', async route => {
      if (!new URL(route.request().url()).searchParams.has('watermark')) return route.continue();
      requests++;
      if (fail && requests === 1) return route.abort();
      return route.continue();
    });
    await page.goto(`${config.url}/discover?seen=true`);
    const first = page.locator('[data-discovery-card]').first();
    await first.getByRole('button', { name: 'Open profile', exact: true }).focus();
    await page.locator('.discovery-pagination').scrollIntoViewIfNeeded();
    if (fail) {
      await expect(page.locator('.discovery-page-status')).toHaveText('Couldn’t load more. Select Load more to try again.');
      await page.mouse.wheel(0, 400);
      await page.waitForTimeout(250);
      assert.equal(requests, 1, 'failed autoload must not loop');
      await page.getByRole('link', { name: 'Next page', exact: true }).click();
    }
    await expect(page.locator('[data-discovery-card]')).toHaveCount(25);
    const ids = await page.locator('[data-discovery-card]').evaluateAll(nodes => nodes.map(node => node.id));
    assert.equal(new Set(ids).size, ids.length, 'no duplicate appended cards');
    if (!fail) assert.equal(await page.evaluate(() => document.activeElement?.textContent), 'Open profile', 'autoload does not steal focus');
    assert.equal(requests, fail ? 2 : 1);
    await context.close();
    console.log(`PASS automatic pagination${fail ? ' with failed load and manual retry' : ' without focus changes'}`);
  }
  assert.deepEqual(await effects(), before, 'reading all views must not import or enqueue anything');
  const context = await session({ javaScriptEnabled: false });
  context.setDefaultTimeout(10000);
  context.setDefaultNavigationTimeout(10000);
  const page = await context.newPage();
  await page.goto(`${config.url}/discover?q=alice`);
  await page.locator(`[data-discovery-card="${config.entry}"]`).getByRole('button', { name: 'Open profile', exact: true }).click();
  await page.waitForURL('**/@alice@one.example');
  assert.equal(await page.locator('form[action$="/follow"]').count(), 1, 'following stays a separate action');
  const after = await effects();
  assert.equal(after[0], before[0] + 1, 'explicit opening imports the selected account');
  assert.deepEqual(after.slice(1, 5), before.slice(1, 5), 'opening creates no posts, follows, notifications or deliveries');
  await context.close();
  console.log('PASS desktop, mobile, 320px, keyboard, no-JavaScript, accessibility, source/operator views and explicit profile opening');
} finally { await browser.close(); }
