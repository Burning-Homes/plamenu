// Progressive enhancement for native live. The HTML management/RTMP path
// works without JavaScript; this path uses the same broadcast and HLS session.
(() => {
  'use strict';
  const root = document.querySelector('[data-live-browser]');
  if (!root) return;
  const camera = root.querySelector('[data-live-camera]');
  const screen = root.querySelector('[data-live-screen]');
  const stopButton = root.querySelector('[data-live-stop]');
  const preview = root.querySelector('[data-live-preview]');
  const status = root.querySelector('[data-live-status]');
  if (!camera || !screen || !stopButton || !preview || !status) return;

  const supported = !!(window.MediaRecorder && window.WebSocket &&
    navigator.mediaDevices && navigator.mediaDevices.getUserMedia);
  root.hidden = false;
  if (!supported) {
    status.textContent = root.dataset.unsupported;
    camera.disabled = true;
    screen.disabled = true;
    return;
  }
  if (!navigator.mediaDevices.getDisplayMedia) screen.disabled = true;

  let stream = null;
  let socket = null;
  let recorder = null;
  let sendChain = Promise.resolve();
  let stopping = false;
  let stopAck = null;

  function tell(message) { status.textContent = message; }
  function buttons(active) {
    camera.disabled = active;
    screen.disabled = active || !navigator.mediaDevices.getDisplayMedia;
    stopButton.hidden = !active;
  }
  function releaseTracks() {
    if (stream) stream.getTracks().forEach(track => track.stop());
    stream = null;
    preview.srcObject = null;
    preview.hidden = true;
  }
  function chosenMime() {
    const candidates = [
      'video/webm;codecs=vp8,opus',
      'video/webm;codecs=vp8',
      'video/webm',
      'video/mp4;codecs=avc1.42E01E,mp4a.40.2',
      'video/mp4',
    ];
    return candidates.find(type => MediaRecorder.isTypeSupported(type));
  }

  function connect(mime) {
    return new Promise((resolve, reject) => {
      const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
      const url = `${protocol}//${location.host}/web/videos/live/${root.dataset.statusId}/publish`;
      socket = new WebSocket(url);
      let settled = false;
      socket.onopen = () => socket.send(JSON.stringify({ csrf: root.dataset.csrf, mime }));
      socket.onmessage = event => {
        let result;
        try { result = JSON.parse(event.data); } catch { result = {}; }
        if (result.ready) { settled = true; resolve(); }
        else if (result.stopped && stopAck) stopAck(true);
        else if (result.error) {
          if (!settled) { settled = true; reject(new Error(result.error)); }
          else { tell(result.error); stop(); }
        }
      };
      socket.onerror = () => {
        if (!settled) { settled = true; reject(new Error(root.dataset.connectionError)); }
        else { tell(root.dataset.connectionError); stop(); }
      };
      socket.onclose = () => {
        if (stopAck) stopAck(false);
        if (!settled) { settled = true; reject(new Error(root.dataset.connectionError)); }
        else if (!stopping) { tell(root.dataset.connectionError); stop(); }
      };
    });
  }

  async function start(kind) {
    buttons(true);
    stopButton.hidden = true;
    stopping = false;
    tell(root.dataset.requesting);
    try {
      if (kind === 'screen') {
        stream = await navigator.mediaDevices.getDisplayMedia({ video: { frameRate: 30 }, audio: true });
      } else {
        stream = await navigator.mediaDevices.getUserMedia({
          video: { width: { ideal: 1280 }, height: { ideal: 720 }, frameRate: { ideal: 30 } },
          audio: true,
        });
      }
      const mime = chosenMime();
      if (!mime) throw new Error(root.dataset.unsupported);
      await connect(mime);
      preview.srcObject = stream;
      preview.hidden = false;
      preview.play().catch(() => {});
      recorder = new MediaRecorder(stream, { mimeType: mime, videoBitsPerSecond: 2500000 });
      sendChain = Promise.resolve();
      recorder.ondataavailable = event => {
        if (!event.data.size) return;
        // Blob reads may finish out of order. Keep the bytes and their WebM /
        // fMP4 headers in recorder order, with a bounded socket backlog.
        sendChain = sendChain.then(async () => {
          if (!socket || socket.readyState !== WebSocket.OPEN) return;
          const bytes = await event.data.arrayBuffer();
          if (socket.bufferedAmount + bytes.byteLength > 8 * 1024 * 1024) {
            tell(root.dataset.slowConnection);
            stop();
            return;
          }
          socket.send(bytes);
        }).catch(() => { tell(root.dataset.connectionError); stop(); });
      };
      recorder.onstop = async () => {
        await sendChain;
        let acknowledged = false;
        if (socket && socket.readyState === WebSocket.OPEN) {
          acknowledged = await new Promise(resolve => {
            const timeout = setTimeout(() => {
              stopAck = null;
              resolve(false);
            }, 5000);
            stopAck = accepted => {
              clearTimeout(timeout);
              stopAck = null;
              resolve(accepted);
            };
            try { socket.send('stop'); } catch { stopAck(false); }
          });
        }
        if (socket) socket.close();
        socket = null;
        recorder = null;
        releaseTracks();
        buttons(false);
        if (status.textContent === root.dataset.onAir) {
          tell(acknowledged ? root.dataset.stopped : root.dataset.connectionError);
        }
      };
      stream.getVideoTracks().forEach(track => track.addEventListener('ended', stop, { once: true }));
      recorder.start(1000);
      buttons(true);
      tell(root.dataset.onAir);
    } catch (error) {
      if (socket) socket.close();
      socket = null;
      recorder = null;
      releaseTracks();
      buttons(false);
      tell(error.message || root.dataset.connectionError);
    }
  }

  function stop() {
    if (stopping) return;
    stopping = true;
    stopButton.disabled = true;
    if (recorder && recorder.state !== 'inactive') recorder.stop();
    else {
      if (socket) socket.close();
      socket = null;
      releaseTracks();
      buttons(false);
    }
    stopButton.disabled = false;
  }

  camera.addEventListener('click', () => start('camera'));
  screen.addEventListener('click', () => start('screen'));
  stopButton.addEventListener('click', stop);
  window.addEventListener('pagehide', stop);
})();
