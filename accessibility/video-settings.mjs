// Exercise the same overflow entry in full pages and embedded frames.
export async function openVideoSettings(page, noScript = false) {
  const modal = page.locator('.video-settings-dialog');
  if (!noScript && await modal.isVisible()) return modal;
  const link = page.locator('[data-video-settings-open]').first();
  const menu = link.locator('xpath=ancestor::details[1]');
  if (!await menu.evaluate(e => e.open)) await menu.locator('summary').press('Enter');
  await link.press('Enter');
  const panel = noScript ? page.locator('.video-settings-page') : modal;
  await panel.waitFor({state:'visible'});
  return panel;
}

export async function closeVideoSettings(page) {
  await page.locator('.video-settings-dialog').getByRole('button', {name:'Close video settings', exact:true}).press('Enter');
  await page.locator('.video-settings-dialog').waitFor({state:'detached'});
}

// Finite VOD puts quality inside its playback disclosure; live/native controls
// can keep it visible. Drive the real disclosure rather than forcing a hidden
// select or changing player internals.
export async function selectVideoQuality(page, label) {
  const quality = page.getByRole('combobox', {name:'Video quality',exact:true,includeHidden:true});
  await quality.waitFor({state:'attached',timeout:20000});
  const menu = quality.locator('xpath=ancestor::details[1]');
  const open = !await quality.isVisible() && await menu.count() === 1;
  if (open) await menu.locator('summary').press('Enter');
  await quality.waitFor({state:'visible',timeout:20000});
  await quality.selectOption({label});
  if (open) await menu.locator('summary').press('Enter');
}
