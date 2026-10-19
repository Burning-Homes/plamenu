import {test,expect} from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import {readFile} from 'node:fs/promises';
import {signIn} from '../session.mjs';

async function journey(page,info) {
  test.setTimeout(180000);
  if(info.project.use.javaScriptEnabled===false) await page.setViewportSize({width:320,height:800});
  await signIn(page);
  await page.goto('/settings/export');
  await page.getByRole('button',{name:'Request archive',exact:true}).press('Enter');
  await expect(page.locator('.settings__saved')).toContainText('Archive requested');
  await expect(page.getByRole('button',{name:'Request archive',exact:true})).toHaveCount(0);
  await expect(page.getByText('You can request a new archive once every',{exact:false})).toBeVisible();
  const downloadLink=page.locator('a[download^="archive-"]');
  await expect.poll(async()=>{
    await page.reload();
    return downloadLink.count();
  },{timeout:120000,intervals:[500,1000,2000]}).toBe(1);
  await expect(downloadLink.locator('..')).toContainText('Ready');
  if(info.project.use.javaScriptEnabled!==false) {
    const scan=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
    expect(scan.violations.map(v=>({id:v.id,targets:v.nodes.map(n=>n.target)}))).toEqual([]);
  }
  const pending=page.waitForEvent('download');
  await downloadLink.press('Enter');
  const download=await pending;
  expect(download.suggestedFilename()).toMatch(/^archive-\d+\.zip$/);
  const path=info.outputPath('fixture-account.zip');
  await download.saveAs(path);
  const buffer=await readFile(path);
  expect(buffer.length).toBeGreaterThan(100);
  expect(buffer.readUInt32LE(0)).toBe(0x04034b50);
  // Archive contents are qualified by the server ZIP tests; never publish this
  // fixture's private archive or raw traces as accessibility review evidence.
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=document.documentElement.clientWidth+1)).toBe(true);
}
test('Full archive request, ready state and download remain accessible',async({page},info)=>journey(page,info));
test('@no-js full archive uses native request and download controls',async({page},info)=>journey(page,info));
