# Webxdc local asset policy check

Package `index.html` and `manifest.toml` at the root of an `.xdc` ZIP and upload
it as an app session. For example, from this directory:

```sh
python3 -m zipfile -c /tmp/webxdc-assets.xdc index.html manifest.toml
```

Opening the app should show five PASS results and a red texture sample. The
fixture loads `webxdc.js`, so asset reads exercise the real bridge as well as CSP:

- A packaged file is readable with `fetch()`.
- A generated `blob:` image is readable with `fetch()` and `createImageBitmap()`.
- A `data:` image is readable with the same texture-loading path.
- A request to the parent Plamenu server is rejected by the bridge.
- A request to the parent Plamenu server is rejected with a
  `connect-src` policy violation. A CORS or DNS failure alone does not verify CSP.

The fixture derives the parent server from Plamenu's
`<session>.webxdc.<domain>` runtime hostname and probes only `/health`, without
credentials. The CSP probe uses native fetch captured before loading the bridge
to check that boundary independently.
