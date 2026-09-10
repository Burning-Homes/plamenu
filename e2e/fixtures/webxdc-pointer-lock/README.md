# Webxdc pointer lock check

Package the fixture from this directory:

```sh
python3 -m zipfile -c /tmp/webxdc-pointer-lock.xdc index.html manifest.toml
```

Upload it as a Webxdc session and open the app. Click **Capture mouse**:
the status must report **PASS: mouse captured**, and mouse movement must
increase the counter even at screen edges. Press Escape to release the mouse;
the status must report **PASS: mouse released**. Repeat in host fullscreen.
If the browser exits fullscreen on Escape, enter fullscreen again before
recapturing. Follow the browser's normal delay/gesture requirements when
recapturing after Escape.

This uses the real Webxdc bridge inside the session iframe. Without
`allow-pointer-lock` in its sandbox, capture fails with a sandbox rejection
even in fullscreen. Verify both account and guest players, which share the
same iframe rendering path.
