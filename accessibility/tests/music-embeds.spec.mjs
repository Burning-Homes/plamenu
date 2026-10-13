import {playNativeAudio} from '../session.mjs';
import {freshMusicAudio} from '../session.mjs';
import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {signIn,openDetailsFor} from '../session.mjs';
import {randomUUID} from 'node:crypto';
import {createServer} from 'node:http';

async function audit(page,noScript){
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
  if(!noScript){
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
    expect(result.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
}
async function journey(page,browser,info,noScript){
  test.setTimeout(180000);
  const title='Embed music '+randomUUID().slice(0,8);
  await signIn(page);
  await page.goto('/music/assets?mine=true');
  const upload=page.locator('form[action="/web/music/assets/import"]');
  await openDetailsFor(upload);
  await upload.getByLabel('Audio file',{exact:true}).setInputFiles(await freshMusicAudio());
  await upload.getByRole('button',{name:'Upload audio',exact:true}).press('Enter');
  await page.waitForURL(url=>/^\/music\/assets\/\d+$/.test(url.pathname));
  const track=page.url();
  const id=new URL(track).pathname.split('/').at(-1);
  const edit=page.locator(`form[action="/web/music/assets/${id}/edit"]`);
  await openDetailsFor(edit);
  await edit.getByLabel('Title',{exact:true}).fill(title);
  await edit.getByLabel('Who can listen',{exact:true}).selectOption('public');
  await edit.getByRole('button',{name:'Save track',exact:true}).press('Enter');
  await openDetailsFor(page.getByRole('link',{name:'Embed music',exact:true,includeHidden:true}));
  await page.getByRole('link',{name:'Embed music',exact:true}).press('Enter');
  await expect(page.locator('#music-embed-code')).toHaveValue(/<audio/);
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('music-embed-code.png'),fullPage:true});
  await page.getByLabel('Embed format',{exact:true}).selectOption(noScript?'iframe':'script');
  await page.getByRole('button',{name:'Show embed code',exact:true}).press('Enter');
  // Map only the fixture's canonical HTTPS origin to its disposable TLS port.
  // The generated snippet, resource paths and external publisher remain real.
  const snippet=(await page.locator('#music-embed-code').inputValue()).replaceAll(
    process.env.A11Y_CANONICAL_ORIGIN+'/',process.env.A11Y_BASE_URL+'/',
  );
  const editor=page.url();
  await page.goto('/music/collections');
  const create=page.locator('form[action="/web/music/collections/create"]');
  await openDetailsFor(create);
  await create.getByLabel('Name',{exact:true}).fill(title+' collection');
  await create.locator('select[name="kind"]').selectOption('playlist');
  await create.locator('select[name="visibility"]').selectOption('public');
  await create.getByRole('button',{name:'Create collection',exact:true}).press('Enter');
  await page.waitForURL(url=>/^\/music\/collections\/\d+$/.test(url.pathname));
  const collectionId=new URL(page.url()).pathname.split('/').at(-1);
  const add=page.locator(`form[action="/web/music/collections/${collectionId}/add"]`);
  await openDetailsFor(add);
  await add.getByLabel('Track, Audio post or podcast episode URL',{exact:true}).fill(track);
  await add.getByRole('button',{name:'Add a track',exact:true}).press('Enter');
  await openDetailsFor(page.getByRole('link',{name:'Embed music',exact:true,includeHidden:true}));
  await page.getByRole('link',{name:'Embed music',exact:true}).press('Enter');
  await expect(page.locator('#music-embed-code')).toHaveValue(/plamenu-music-list/);
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('music-collection-embed-code.png'),fullPage:true});
  await page.getByRole('link',{name:'Preview player',exact:true}).press('Enter');
  await expect(page.getByRole('heading',{name:title+' collection',exact:true})).toBeVisible();
  await expect(page.locator('audio')).toHaveCount(1);
  await audit(page,noScript);
  await page.screenshot({path:info.outputPath('music-collection-embed-player.png'),fullPage:true});
  await page.goto(editor);
  const server=createServer((request,response)=>{
    response.writeHead(200,{'Content-Type':'text/html; charset=utf-8'});
    response.end(`<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>External music publisher</title><style>body{font:16px system-ui;margin:16px}iframe{border:0;max-width:100%}</style><h1>External music publisher</h1>${snippet}</html>`);
  });
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  const external=await browser.newContext({baseURL:process.env.A11Y_BASE_URL,ignoreHTTPSErrors:false,javaScriptEnabled:!noScript,extraHTTPHeaders:{},viewport:{width:320,height:800}});
  try{
    const guest=await external.newPage();
    await guest.goto(`http://127.0.0.1:${server.address().port}/`);
    const player=guest.frameLocator('iframe');
    await expect(player.getByRole('heading',{name:title,exact:true})).toBeVisible();
    await expect(player.locator('input[name="csrf"],form.logout')).toHaveCount(0);
    await expect(player.locator('audio')).toHaveAttribute('src',`/music/embed/assets/${id}/audio`);
    const frame=guest.frames().find(frame=>frame.url().includes('/music/embed/'));
    await audit(frame,true);
    await audit(guest,noScript);
    await guest.screenshot({path:info.outputPath('music-external-widget.png'),fullPage:true});
    if(!noScript){
      await playNativeAudio(player.locator('audio'),info);
      await expect.poll(()=>player.locator('audio').evaluate(a=>a.currentTime),{timeout:15000}).toBeGreaterThan(1);
    }
    const stream=new URL(`/music/embed/assets/${id}/audio`,process.env.A11Y_BASE_URL).href;
    if(!noScript){
      const status=await guest.evaluate(async url=>(await fetch(url,{credentials:'omit',headers:{Range:'bytes=0-31'}})).status,stream);
      expect(status).toBe(206);
    }
    const bytes=await external.request.get(stream,{headers:{Range:'bytes=0-31'}});
    expect(bytes.status()).toBe(206);
    await page.goto(track);
    const privateEdit=page.locator(`form[action="/web/music/assets/${id}/edit"]`);
    await openDetailsFor(privateEdit);
    await privateEdit.getByLabel('Who can listen',{exact:true}).selectOption('private');
    await Promise.all([page.waitForNavigation({waitUntil:'domcontentloaded'}),privateEdit.getByRole('button',{name:'Save track',exact:true}).press('Enter')]);
    expect((await external.request.get(stream,{headers:{Range:'bytes=0-31','If-None-Match':'*'}})).status()).toBe(404);
    if(!noScript){
      const status=await guest.evaluate(async url=>(await fetch(url,{credentials:'omit',headers:{Range:'bytes=0-31'}})).status,stream);
      expect(status).toBe(404);
    }

    expect((await external.request.get(new URL(`/music/embed/assets/${id}`,process.env.A11Y_BASE_URL).href)).status()).toBe(404);
  }finally{
    await external.close();
    await new Promise((resolve,reject)=>server.close(error=>error?reject(error):resolve()));
  }
}
test('public music widget plays on a real external origin',async({page,browser},info)=>journey(page,browser,info,false));
test('public music iframe works without JavaScript @no-js',async({page,browser},info)=>journey(page,browser,info,true));
