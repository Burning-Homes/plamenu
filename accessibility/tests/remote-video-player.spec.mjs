import {test,expect} from '@playwright/test';
import {signIn} from '../session.mjs';

// Opt-in local federation drill, reusing the disposable runner's explicit
// peer URL and private-address permission. No mocked metadata or media bytes.
test('remote PeerTube video uses the same player without a local storyboard',async({page},info)=>{
  test.skip(!process.env.A11Y_IMPORT_SOURCE_URL,'Requires an explicit public PeerTube fixture video.');
  test.setTimeout(120000);
  await signIn(page);
  await page.goto('/search?q='+encodeURIComponent(process.env.A11Y_IMPORT_SOURCE_URL));
  const video=page.locator('.status video').first();
  await expect(video).toBeVisible();
  const card=video.locator('xpath=ancestor::article').first();
  await expect(card.locator('.vod-controls')).toHaveCount(1);
  const previews=card.locator('[data-vod-controls]');
  expect(JSON.parse(await previews.getAttribute('data-vod-controls')).url).toBeUndefined();
  await card.locator('.vod-controls__play').click();
  await expect.poll(()=>video.evaluate(v=>v.currentTime),{timeout:30000}).toBeGreaterThan(.25);
  expect(await video.evaluate(v=>v.error?.code??null)).toBeNull();
  expect(await video.evaluate(v=>v.videoWidth)).toBeGreaterThan(0);
  await video.evaluate(v=>v.pause());
  for(const width of info.project.name.includes('mobile')?[320,390]:[768,1440]){
    await page.setViewportSize({width,height:900});
    await video.scrollIntoViewIfNeeded();
    await page.screenshot({path:info.outputPath(`remote-peertube-${width}.png`)});
    expect(await page.evaluate(()=>document.documentElement.scrollWidth)).toBeLessThanOrEqual(width);
  }
});
