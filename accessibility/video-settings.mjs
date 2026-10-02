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
