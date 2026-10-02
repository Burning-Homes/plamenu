// PeerTube's administrator welcome wizard can arrive after login/navigation.
// Dismiss this specific setup overlay through its real, non-persistent action.
// Playwright includes handler execution in the triggering action's timeout and
// waits for the overlay to hide before retrying the original action.
export async function installPeerTubeWelcomeHandler(page) {
  const welcome = page.locator('ngb-modal-window').filter({
    has: page.locator('my-admin-config-wizard-welcome'),
  });
  await page.addLocatorHandler(welcome, async dialog => {
    await dialog.getByRole('button', { name: 'Remind me later', exact: true }).click();
  });
}
