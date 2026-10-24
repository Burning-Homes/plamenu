import {playNativeAudio} from '../session.mjs';
import {freshMusicAudio} from '../session.mjs';
import {test,expect} from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import {signIn,openDetailsFor,openMusicUpload} from '../session.mjs';
test.use({coldRendition:true});
async function journey(page,browser,coldEncoding,info,shared,format=shared?'aac':'mp3'){
  test.setTimeout(180000);
  await signIn(page);await openMusicUpload(page);
  const upload=page.locator('form[action="/web/music/assets/import"]');await openDetailsFor(upload);
  await upload.getByLabel('Audio file',{exact:true}).setInputFiles(await freshMusicAudio(30));
  await upload.getByRole('button',{name:'Save privately',exact:true}).press('Enter');
  await page.waitForURL(url=>/^\/music\/assets\/\d+$/.test(url.pathname));
  const id=new URL(page.url()).pathname.split('/').at(-1);
  let context=null;let player=page;
  try{
    if(shared){
      await page.getByRole('link',{name:'Create share link',exact:true}).press('Enter');
      await page.getByRole('button',{name:'Create share link',exact:true}).press('Enter');
      const link=await page.locator('#music-share-url').inputValue();
      context=await browser.newContext({baseURL:process.env.A11Y_BASE_URL,ignoreHTTPSErrors:false,javaScriptEnabled:false,extraHTTPHeaders:{},viewport:{width:320,height:800}});
      player=await context.newPage();await player.goto(link);
    }
    await openDetailsFor(player.getByRole('link',{name:'Playback formats',exact:true,includeHidden:true}));
    await player.getByRole('link',{name:'Playback formats',exact:true}).press('Enter');
    const formats=player.url();const form=player.locator(`form[action="/web/music/assets/${id}/renditions"]`);
    await form.getByLabel('Audio format',{exact:true}).selectOption(format);
    await form.getByLabel('Bitrate (kbps)',{exact:true}).fill('320');
    await form.getByRole('button',{name:'Play now',exact:true}).press('Enter');
    await expect(player.locator('audio')).toHaveAttribute('preload','none');
    if(format==='flac')await expect(player.getByText('FLAC · Lossless',{exact:true})).toBeVisible();else await expect(player.locator('[data-bitrate-limit]')).toHaveAttribute('data-bitrate-limit','192');
    const rowState=async()=>{const html=await(await player.request.get(formats)).text();return await player.evaluate(({html,format})=>new DOMParser().parseFromString(html,'text/html').querySelector(`li[data-rendition-format="${format}"]`)?.textContent??'',{html,format});};
    expect(await rowState()).toBe('');
    await player.screenshot({path:info.outputPath('music-cold-player-before-play.png'),fullPage:true});
    await playNativeAudio(player.locator('audio'),info);
    await expect.poll(()=>player.locator('audio').evaluate(audio=>audio.currentTime),{timeout:20000}).toBeGreaterThan(1);
    const during=await rowState();
    expect(during).toContain('Encoding');expect(during).not.toContain('Ready to play');
    expect(await player.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
    if(!shared){const result=await new AxeBuilder({page:player}).withTags(['wcag2a','wcag2aa','wcag21aa','wcag22aa']).analyze();expect(result.violations).toEqual([]);}
    await player.screenshot({path:info.outputPath('music-cold-player-during-encoding.png'),fullPage:true});
    await coldEncoding.release();
    await expect.poll(rowState,{timeout:45000}).toContain('Ready to play');
    const src=await player.locator('audio').getAttribute('src');
    const range=await player.request.get(src,{headers:{Range:'bytes=0-31'}});expect(range.status()).toBe(206);
    await player.goto(formats);
    await player.screenshot({path:info.outputPath('music-cold-player-ready-cache.png'),fullPage:true});
  }finally{if(context)await context.close();}
}
test('music starts playing while the real cold encoder is running',async({page,browser,coldEncoding},info)=>journey(page,browser,coldEncoding,info,false));
test('shared AAC cold playback works without JavaScript @no-js',async({page,browser,coldEncoding},info)=>journey(page,browser,coldEncoding,info,true));

for(const format of ['ogg','flac','opus'])test(`remaining ${format} cold format decodes during encoding`,async({page,browser,coldEncoding},info)=>journey(page,browser,coldEncoding,info,false,format));
