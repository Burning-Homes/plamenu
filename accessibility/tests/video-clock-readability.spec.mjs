import {test,expect} from '../fixtures.mjs';
import {randomUUID} from 'node:crypto';
import {writeFile} from 'node:fs/promises';
import AxeBuilder from '@axe-core/playwright';
import {signIn,openVideoOptions,videoChannelForm,createVideoChannel,chooseVideoLanguage} from '../session.mjs';

async function textSpacing(page,active) {
  await page.evaluate(active=>{
    const previous=window.clockSpacingRules;
    if(previous) {
      for(let n=previous.count-1;n>=0;n--) previous.sheet.deleteRule(previous.start+n);
      window.clockSpacingRules=null;
    }
    if(active) {
      const sheet=[...document.styleSheets].find(s=>s.href?.includes('/assets/app.css'));
      if(!sheet) throw new Error('Application stylesheet unavailable');
      const rules=['body * {line-height:1.5!important;letter-spacing:.12em!important;word-spacing:.16em!important;}','body p {margin-block-end:2em!important;}'];
      const start=sheet.cssRules.length;
      for(const rule of rules) sheet.insertRule(rule,sheet.cssRules.length);
      window.clockSpacingRules={sheet,start,count:rules.length};
    }
  },active);
}

test('Published video clock stays readable at narrow widths and text spacing',async({page},info)=>{
  test.setTimeout(180000);
  await signIn(page);
  await page.goto('/videos/upload');
  await openVideoOptions(page);
  const form=await videoChannelForm(page);
  const name=`clock_${randomUUID().replaceAll('-','').slice(0,16)}`;
  await form.getByLabel('Channel address').fill(name);
  await form.getByLabel('Channel name').fill('Clock readability fixture');
  await createVideoChannel(page,form.getByRole('button',{name:'Create a video channel',exact:true}));
  await openVideoOptions(page);
  const upload=page.locator('form[action="/web/videos"]');
  await upload.getByRole('combobox',{name:'Channel',exact:true}).selectOption({label:'Clock readability fixture'});
  await upload.getByLabel('Video title',{exact:true}).fill('Readable playback time');
  await chooseVideoLanguage(upload,'en');
  await upload.getByLabel('Video or audio file',{exact:true}).setInputFiles(process.env.A11Y_VIDEO_FIXTURE);
  // Wait for the draft document, not WebKit's media-dependent load event.
  await Promise.all([
    page.waitForURL(url=>/^\/videos\/drafts\/\d+$/.test(url.pathname),{waitUntil:'domcontentloaded'}),
    upload.getByRole('button',{name:'Upload video',exact:true}).press('Enter',{noWaitAfter:true}),
  ]);
  await expect(page.getByRole('link',{name:'Review your video',exact:true})).toBeVisible({timeout:60000});
  await page.getByRole('link',{name:'Review your video',exact:true}).press('Enter');
  await page.getByRole('button',{name:'Publish video',exact:true}).press('Enter');
  await expect(page).toHaveURL(url=>/^\/@[^/]+\/\d+$/.test(url.pathname));
  const figure=page.locator('figure.media--video').first();
  const video=figure.locator('video');
  const clock=figure.locator('.vod-controls__time');
  await expect(clock).toContainText('/');
  await video.evaluate(v=>{v.muted=true;});
  await figure.getByRole('button',{name:'Play',exact:true}).press('Enter');
  await expect.poll(()=>video.evaluate(v=>v.currentTime)).toBeGreaterThan(.1);
  await figure.getByRole('button',{name:'Pause',exact:true}).press('Enter');
  await expect.poll(()=>video.evaluate(v=>v.paused)).toBe(true);
  const geometry=[];
  for(const [width,label] of [[1440,'desktop'],[320,'mobile']]) {
    await page.setViewportSize({width,height:900});
    for(const spaced of [false,true]) {
      await textSpacing(page,spaced);
      const data=await figure.locator('.vod-player').evaluate(player=>{
        const clock=player.querySelector('.vod-controls__time');
        const box=player.getBoundingClientRect();
        return {viewport:document.documentElement.clientWidth,document:document.documentElement.scrollWidth,
          text:clock.textContent,clockWidth:clock.clientWidth,clockContent:clock.scrollWidth,
          letterSpacing:getComputedStyle(clock).letterSpacing,
          targets:[...player.querySelectorAll('.vod-controls button,.vod-controls summary')].filter(e=>e.checkVisibility()).map(e=>{
            const r=e.getBoundingClientRect();
            return {name:e.getAttribute('aria-label'),width:r.width,height:r.height,inside:r.left>=box.left-1&&r.right<=box.right+1};
          })};
      });
      geometry.push({width,spaced,...data});
      await figure.locator('.vod-player').screenshot({path:info.outputPath(`video-clock-${label}-${spaced?'spaced':'default'}.png`),animations:'disabled'});
      expect.soft(data.clockContent,JSON.stringify(data)).toBeLessThanOrEqual(data.clockWidth+1);
      expect.soft(data.document).toBeLessThanOrEqual(data.viewport+1);
      expect.soft(data.targets.every(t=>t.inside&&t.width>=44&&t.height>=44)).toBe(true);
      const scan=await new AxeBuilder({page}).include('.vod-controls').withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
      expect.soft(scan.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
    }
  }
  await writeFile(info.outputPath('clock-geometry.json'),JSON.stringify(geometry,null,2));
});
