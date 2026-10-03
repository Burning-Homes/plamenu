import {test, expect} from '@playwright/test';
import {readFileSync} from 'node:fs';
import {signIn, openVideoOptions, videoChannelForm, createVideoChannel} from '../session.mjs';

test.use({serviceWorkers:'allow'});
async function qualification(page, info, noScript) {
  await signIn(page);
  expect(info.project.use.ignoreHTTPSErrors).toBe(false);
  const cookies = await page.context().cookies();
  const session = cookies.find(cookie => cookie.name === '__Host-plamenu_session');
  expect(session).toMatchObject({secure:true,httpOnly:true,path:'/'});
  expect(new URL(page.url()).protocol).toBe('https:');
  if (!noScript) {
    await page.evaluate(async () => {
      await Promise.race([navigator.serviceWorker.ready,new Promise((_,reject)=>setTimeout(()=>reject(new Error('Service worker not ready')),10000))]);
    });
    await page.reload();
    expect(await page.evaluate(()=>Boolean(navigator.serviceWorker.controller))).toBe(true);
  }
  await page.goto('/videos/upload'); await openVideoOptions(page);
  const name = `tls_${Date.now().toString(36)}_${info.project.name.replace(/[^a-z]/g,'').slice(0,18)}`.slice(0,30);
  const create = await videoChannelForm(page);
  if (!await create.isVisible()) await create.locator('..').locator(':scope > summary').click();
  await create.getByLabel('Channel address').fill(name);
  await expect(create.getByLabel('Channel address')).toHaveValue(name);
  await create.getByLabel('Channel name').fill(`TLS channel ${name}`);
  await createVideoChannel(page, create.getByRole('button',{name:'Create a video channel'}));
  await page.goto('/videos/channels');
  await page.getByRole('link',{name:`TLS channel ${name}`,exact:true}).click();
  const form = page.locator('form[action$="/profile"]');
  const png = readFileSync(process.env.A11Y_POSTER_FIXTURE);
  for (const kind of ['avatar','header']) {
    await form.locator(`input[name="${kind}"]`).setInputFiles({name:`${kind}.png`,mimeType:'image/png',buffer:png});
    await form.locator(`input[name="${kind}_description"]`).fill(`${name} ${kind} first`);
  }
  await form.getByRole('button',{name:'Save channel profile',exact:true}).click();
  await page.goto(`/@${name}`);
  const avatar = page.locator(`img.profile__avatar[alt="${name} avatar first"]`);
  const header = page.locator(`img[alt="${name} header first"]`);
  for (const image of [avatar,header]) {
    await image.scrollIntoViewIfNeeded();
    await expect.poll(()=>image.evaluate(img=>img.complete&&img.naturalWidth>0)).toBe(true);
    expect(new URL(await image.getAttribute('src'),page.url()).origin).toBe(new URL(page.url()).origin);
  }
  const firstURL = await avatar.getAttribute('src');
  // A second actual upload must invalidate the immutable media URL and the
  // active service worker must not restore the old profile after reload.
  await page.goto('/videos/channels');
  await page.getByRole('link',{name:`TLS channel ${name}`,exact:true}).click();
  await form.locator('input[name="avatar"]').setInputFiles({name:'fresh.png',mimeType:'image/png',buffer:png});
  await form.locator('input[name="avatar_description"]').fill(`${name} avatar fresh`);
  await form.getByRole('button',{name:'Save channel profile',exact:true}).click();
  await page.goto(`/@${name}`); await page.reload();
  const fresh = page.locator(`img.profile__avatar[alt="${name} avatar fresh"]`);
  await expect.poll(()=>fresh.evaluate(img=>img.complete&&img.naturalWidth>0)).toBe(true);
  expect(await fresh.getAttribute('src')).not.toBe(firstURL);
  if (!noScript) expect(await page.evaluate(()=>Boolean(navigator.serviceWorker.controller))).toBe(true);
  await page.screenshot({path:info.outputPath('verified-https-channel-media.png'),fullPage:true});
}
test('verified HTTPS auth and channel images survive service worker reload', async ({page},info)=>qualification(page,info,false));
test('verified HTTPS auth and channel images retain native forms @no-js', async ({page},info)=>qualification(page,info,true));
