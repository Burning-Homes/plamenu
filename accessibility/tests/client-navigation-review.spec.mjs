import {test,expect} from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import {signIn} from '../session.mjs';

test('connected client screens retain their hierarchy at phone and desktop sizes',async({page},info)=>{
  test.skip(info.project.name!=='chromium-desktop','Visual catalogue is recorded once; the individual journeys cover each engine.');
  test.setTimeout(240000);
  await signIn(page);
  const paths=[['music','/music'],['library','/music/mine'],['albums','/music/albums'],['artists','/music/artists'],['radio','/music/streams'],['podcasts','/music/podcasts'],['upload-audio','/music/assets?mine=true&upload=true'],['messages-empty','/conversations'],['new-message','/compose?visibility=direct'],['compose','/compose'],['videos','/videos/explore'],['upload-video','/videos/upload'],['start-live','/videos/live/new']];
  const failures=[];
  for(const [name,path] of paths){
    const response=await page.goto(path,{waitUntil:'domcontentloaded'});
    expect(response.status()).toBe(200);
    for(const width of [390,1440]){
      await page.setViewportSize({width,height:900});
      await page.emulateMedia({colorScheme:width===390?'dark':'light'});
      await page.screenshot({path:info.outputPath(`${name}-${width}.png`),fullPage:true,animations:'disabled',timeout:30000});
      expect(await page.evaluate(()=>document.documentElement.scrollWidth)).toBeLessThanOrEqual(width);
      const scan=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();
      if(scan.violations.length)failures.push({name,width,violations:scan.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))});
    }
  }
  expect(failures).toEqual([]);
});
