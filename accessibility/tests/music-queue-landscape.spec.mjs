import {test,expect} from '../fixtures.mjs';
import {signIn,freshMusicAudio,openDetailsFor} from '../session.mjs';
import {writeFile} from 'node:fs/promises';

async function journey(page,info,height) {
  test.setTimeout(120000);
  await page.setViewportSize({width:800,height});
  await signIn(page);
  await page.goto('/music/assets?upload=true');
  const form=page.locator('form.music-upload');
  await form.locator('[name=file]').setInputFiles(await freshMusicAudio());
  await form.locator('[name=title]').fill('Landscape queue fixture');
  await form.locator('summary').press('Enter');
  await form.locator('[name=album]').fill('Landscape queue album');
  await form.locator('button[value=publish]').press('Enter');
  await expect(page).toHaveURL(url=>/^\/music\/assets\/\d+$/.test(url.pathname));
  await page.getByRole('link',{name:'Landscape queue album',exact:true}).press('Enter');
  await page.getByRole('button',{name:'Play these tracks',exact:true}).press('Enter');
  const dock=page.locator('.music-dock');
  await expect(dock).toBeVisible();
  const upload=page.locator('main').getByRole('link',{name:'Upload audio',exact:true,includeHidden:true});
  await openDetailsFor(upload);
  await upload.press('Enter');
  await expect(form).toBeVisible();
  await dock.locator('[data-music-queue-toggle]').press('Enter');
  await form.locator('[name=file]').focus();
  await page.keyboard.press('Tab');
  await page.evaluate(()=>new Promise(requestAnimationFrame));
  await expect(form.locator('[name=title]')).toBeFocused();
  const geometry=await page.evaluate(()=>{
    const field=document.activeElement.getBoundingClientRect(),dock=document.querySelector('.music-dock').getBoundingClientRect();
    return {top:field.top,bottom:field.bottom,dockTop:dock.top,dockBottom:dock.bottom,height:innerHeight};
  });
  await writeFile(info.outputPath('music-queue-landscape-geometry.json'),JSON.stringify(geometry,null,2)+'\n');
  await page.screenshot({path:info.outputPath('music-queue-landscape.png')});
  expect.soft(geometry.top).toBeGreaterThanOrEqual(0);
  expect.soft(geometry.bottom).toBeLessThanOrEqual(geometry.dockTop+1);
  expect.soft(geometry.dockTop).toBeGreaterThanOrEqual(0);
  const play=dock.getByRole('button',{name:'Play this track',exact:true,includeHidden:true}).first();
  await play.focus();
  await page.evaluate(()=>new Promise(requestAnimationFrame));
  const queueControl=await play.evaluate(e=>{
    const control=e.getBoundingClientRect(),queue=e.closest('section.music-player').getBoundingClientRect();
    return {top:control.top,bottom:control.bottom,queueTop:queue.top,queueBottom:queue.bottom};
  });
  await writeFile(info.outputPath('music-queue-landscape-control-geometry.json'),JSON.stringify(queueControl,null,2)+'\n');
  await page.screenshot({path:info.outputPath('music-queue-landscape-control.png')});
  expect.soft(queueControl.top).toBeGreaterThanOrEqual(queueControl.queueTop-1);
  expect.soft(queueControl.bottom).toBeLessThanOrEqual(queueControl.queueBottom+1);
  await play.press('Enter');
  await expect.poll(()=>dock.locator('audio').evaluate(a=>a.currentTime)).toBeGreaterThan(0);
  await dock.getByRole('button',{name:/^Close player/}).press('Enter');
  await expect(dock).toBeHidden();
}
for(const height of [360,320]) {
  test(`an expanded music queue leaves room for keyboard editing at ${height}px height`,async({page},info)=>journey(page,info,height));
}
