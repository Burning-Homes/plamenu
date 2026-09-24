import { expect, test } from '@playwright/test';

test('native tracker refuses arbitrary swarms before allocating a peer', async ({ page }) => {
  await page.goto('/');
  const result = await page.evaluate(() => new Promise((resolve, reject) => {
    const socket = new WebSocket(`wss://${location.host}/tracker/socket`);
    const timeout = setTimeout(() => { socket.close(); reject(new Error('tracker did not respond')); }, 5000);
    let failure;
    socket.onopen = () => socket.send(JSON.stringify({
      action: 'announce', info_hash: 'AAAAAAAAAAAAAAAAAAAA',
      peer_id: 'ABCDEFGHIJKLMNOPQRST', event: 'started', left: 0, offers: [],
    }));
    socket.onmessage = event => { failure = JSON.parse(event.data)['failure reason']; };
    socket.onclose = () => { clearTimeout(timeout); resolve({ failure, closed: true }); };
    socket.onerror = () => { clearTimeout(timeout); reject(new Error('tracker socket failed')); };
  }));
  expect(result).toEqual({ failure: 'unknown info_hash', closed: true });
});
