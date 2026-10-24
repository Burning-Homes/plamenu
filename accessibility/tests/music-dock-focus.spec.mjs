import {test,expect} from '../fixtures.mjs';
import {signIn,freshMusicAudio,openDetailsFor} from '../session.mjs';
import {writeFile} from 'node:fs/promises';

async function journey(page,info,expanded) {
  test.setTimeout(120000);
  await signIn(page);
  await page.goto('/music/assets?upload=true');
  const form=page.locator('form.music-upload');
  await form.locator('[name=file]').setInputFiles(await freshMusicAudio());
  await form.locator('[name=title]').fill('Dock focus fixture');
  await form.locator('summary').press('Enter');
  await form.locator('[name=album]').fill('Dock focus album');
  await form.locator('button[value=publish]').press('Enter');
  await expect(page).toHaveURL(url=>/^\/music\/assets\/\d+$/.test(url.pathname));
  await page.getByRole('link',{name:'Dock focus album',exact:true}).press('Enter');
  await page.getByRole('button',{name:'Play these tracks',exact:true}).press('Enter');
  const dock=page.locator('.music-dock');
  await expect(dock).toBeVisible();
  const upload=page.locator('main').getByRole('link',{name:'Upload audio',exact:true,includeHidden:true});
  await openDetailsFor(upload);
  await upload.press('Enter');
  await expect(form).toBeVisible();
  await expect(dock).toBeVisible();
  await form.locator('summary').press('Enter');
  if(expanded) await dock.locator('[data-music-queue-toggle]').press('Enter');
  await form.locator('[name=file]').focus();
  const positions=[];
  for(let step=0;step<6;step++) {
    await page.keyboard.press('Tab');
    await page.evaluate(()=>new Promise(requestAnimationFrame));
    const geometry=await page.evaluate(()=>{
      const control=document.activeElement,box=control.getBoundingClientRect(),dock=document.querySelector('.music-dock').getBoundingClientRect();
      return {tag:control.tagName,name:control.getAttribute('name'),value:control.tagName==='BUTTON'?control.getAttribute('value'):null,
        top:box.top,bottom:box.bottom,dockTop:dock.top,dockBottom:dock.bottom,height:innerHeight};
    });
    positions.push(geometry);
    if(geometry.name==='album') await page.screenshot({path:info.outputPath('music-dock-focus.png')});
    expect.soft(geometry.bottom,JSON.stringify(geometry)).toBeLessThanOrEqual(geometry.dockTop+1);
    expect.soft(geometry.top,JSON.stringify(geometry)).toBeGreaterThanOrEqual(0);
  }
  await writeFile(info.outputPath('music-dock-focus-geometry.json'),JSON.stringify(positions,null,2)+'\n');
  expect(positions.some(p=>p.value==='publish')).toBe(true);
  const sidebar=page.locator('.sidebar');
  if(await sidebar.isVisible()) {
    await sidebar.locator('.brand').focus();
    const navigation=[];
    for(let step=0;step<30;step++) {
      await page.keyboard.press('Tab');
      await page.evaluate(()=>new Promise(requestAnimationFrame));
      const bounds=await page.evaluate(()=>{
        const e=document.activeElement,b=e.getBoundingClientRect(),dock=document.querySelector('.music-dock').getBoundingClientRect();
        return {inSidebar:!!e.closest('.sidebar'),logout:e.classList.contains('logout__btn'),
          text:e.getAttribute('title')||e.textContent.trim().slice(0,70),top:b.top,bottom:b.bottom,dockTop:dock.top};
      });
      navigation.push(bounds);
      expect.soft(bounds.inSidebar).toBe(true);
      expect.soft(bounds.bottom,JSON.stringify(bounds)).toBeLessThanOrEqual(bounds.dockTop+1);
      expect.soft(bounds.top,JSON.stringify(bounds)).toBeGreaterThanOrEqual(0);
      if(bounds.logout) {
        await page.screenshot({path:info.outputPath('music-dock-sidebar.png')});
        break;
      }
    }
    expect(navigation.at(-1).logout).toBe(true);
    await writeFile(info.outputPath('music-dock-sidebar-geometry.json'),JSON.stringify(navigation,null,2)+'\n');
  }
  await dock.getByRole('button',{name:/^Close player/}).press('Enter');
  await expect(dock).toBeHidden();
  await expect(page.locator('body')).not.toHaveClass(/has-music-dock/);
  expect(await page.evaluate(()=>document.documentElement.style.getPropertyValue('--music-dock-clearance'))).toBe('');
}
test('keyboard focus stays above the music dock while editing an upload',async({page},info)=>journey(page,info,false));
test('keyboard focus stays above an expanded music queue while editing an upload',async({page},info)=>journey(page,info,true));
