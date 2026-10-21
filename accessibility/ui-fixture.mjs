import { readFileSync } from 'node:fs';

// Render a small web-UI fixture with the shipping assets, without a database.
// Server tests separately verify that these controls are emitted by the views.
export async function uiFixture(page, body, media = {}) {
  const origin = 'https://plamenu-layout.test';
  const assets = new Map(['app.css', 'app.js'].map(name => [name,
    readFileSync(new URL(`../crates/server/src/web/assets/${name}`, import.meta.url), 'utf8')]));
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.context().route(`${origin}/**`, route => {
    const path = new URL(route.request().url()).pathname;
    if (media[path]) return route.fulfill(media[path]);
    const asset = path.split('/').at(-1);
    if (assets.has(asset)) return route.fulfill({ contentType: asset.endsWith('.css') ? 'text/css' : 'text/javascript', body: assets.get(asset) });
    return route.fulfill({ contentType: 'text/html', body: `<!doctype html><html lang="en"><head>
      <meta name="viewport" content="width=device-width,initial-scale=1"><title>Layout regression</title>
      <link rel="stylesheet" href="/app.css"><script>document.documentElement.classList.add('js')</script>
      <script defer src="/app.js"></script></head><body>${body}</body></html>` });
  });
  await page.goto(origin);
  return errors;
}
