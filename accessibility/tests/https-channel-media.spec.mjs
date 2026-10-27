import {test, expect} from '../fixtures.mjs';
import {readFileSync} from 'node:fs';
import {signIn, openVideoOptions, videoChannelForm, createVideoChannel, freshMusicAudio, openDetailsFor,openMusicUpload} from '../session.mjs';

test.use({serviceWorkers:'allow'});
async function saveChannelProfile(page, form) {
  const action = new URL(await form.getAttribute('action'), page.url()).href;
  // WebKit can finish click's navigation wait before a service-worker-controlled
  // form starts its POST. Wait for the committed save before leaving the page.
  const [response] = await Promise.all([
    page.waitForResponse(response => response.url() === action && response.request().method() === 'POST'),
    form.getByRole('button', {name:'Save channel profile', exact:true}).click(),
  ]);
  expect(response.status()).toBe(303);
}
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
  await saveChannelProfile(page, form);
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
  await saveChannelProfile(page, form);
  await page.goto(`/@${name}`); await page.reload();
  const fresh = page.locator(`img.profile__avatar[alt="${name} avatar fresh"]`);
  await expect.poll(()=>fresh.evaluate(img=>img.complete&&img.naturalWidth>0)).toBe(true);
  expect(await fresh.getAttribute('src')).not.toBe(firstURL);
  if (!noScript) expect(await page.evaluate(()=>Boolean(navigator.serviceWorker.controller))).toBe(true);
  await page.screenshot({path:info.outputPath('verified-https-channel-media.png'),fullPage:true});
}
test('verified HTTPS auth and channel images survive service worker reload', async ({page},info)=>qualification(page,info,false));
test('verified HTTPS auth and channel images retain native forms @no-js', async ({page},info)=>qualification(page,info,true));

test('Music private pages stay out of the offline cache and recover online', async ({page},info)=>{
  await signIn(page);
  await page.evaluate(async()=>{
    await Promise.race([navigator.serviceWorker.ready,new Promise((_,reject)=>setTimeout(()=>reject(new Error('Service worker not ready')),10000))]);
  });
  await page.reload();
  expect(await page.evaluate(()=>Boolean(navigator.serviceWorker.controller))).toBe(true);
  await openMusicUpload(page);
  const form=page.locator('form[action="/web/music/assets/import"]');
  await openDetailsFor(form);
  await form.getByLabel('Audio file',{exact:true}).setInputFiles(await freshMusicAudio());
  await form.getByRole('button',{name:'Save privately',exact:true}).press('Enter');
  await page.waitForURL(url=>/^\/music\/assets\/\d+$/.test(url.pathname));
  const track=page.url();const title=await page.getByRole('heading',{level:1}).innerText();
  await expect(page.locator('section.music-library > p').filter({hasText:/^Only me and people I share with$/})).toBeVisible();
  expect(process.env.A11Y_OFFLINE_CONTROL_URL,'Enable the owned origin outage with A11Y_OFFLINE_FIXTURE=1').toBeTruthy();
  const availability=async mode=>{
    const response=await fetch(process.env.A11Y_OFFLINE_CONTROL_URL+'/'+mode,{method:'POST'});
    expect(response.status).toBe(204);
  };
  const workerProof=await page.evaluate(async()=>({controller:navigator.serviceWorker.controller?.state,scope:(await navigator.serviceWorker.getRegistration())?.scope,offlineCached:Boolean(await caches.match('/offline'))}));
  expect(workerProof).toMatchObject({controller:'activated',offlineCached:true});
  await openDetailsFor(page.getByRole('link',{name:'Activities',exact:true,includeHidden:true}));
  try{
    await availability('offline');
    const shell=await page.evaluate(async()=>fetch('/assets/app.css',{cache:'no-store'}).then(r=>({status:r.status})).catch(e=>({error:e.name})));
    expect(shell,'The actual worker must serve its cached shell during the outage').toEqual({status:200});
    // Navigate through the native resource link during a real origin outage.
    await page.getByRole('link',{name:'Activities',exact:true}).click();
    await expect(page.getByRole('heading',{name:"You're offline",exact:true})).toBeVisible();
    await expect(page.getByRole('heading',{name:title,exact:true})).toHaveCount(0);
    const cached=await page.evaluate(async()=>{
      const urls=[];for(const name of await caches.keys()){for(const request of await(await caches.open(name)).keys())urls.push(request.url);}return urls;
    });
    expect(cached.some(value=>new URL(value).pathname.startsWith('/music/'))).toBe(false);
    expect(cached.some(value=>new URL(value).pathname.startsWith('/api/'))).toBe(false);
    await page.screenshot({path:info.outputPath('music-private-offline.png'),fullPage:true});
  }finally{await availability('online');}
  await page.goto(track);
  await expect(page.getByRole('heading',{name:title,exact:true})).toBeVisible();
});
