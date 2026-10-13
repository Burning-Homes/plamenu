// Progressive enhancement for native live. The HTML management/RTMP path
// works without JavaScript; this path uses the same broadcast and HLS session.
(() => {
  'use strict';
  const root = document.querySelector('[data-live-browser]');
  if (!root) return;
  const camera = root.querySelector('[data-live-camera]');
  const screen = root.querySelector('[data-live-screen]');
  const stopButton = root.querySelector('[data-live-stop]');
  const startButton = root.querySelector('[data-live-start]');
  const cancelButton = root.querySelector('[data-live-cancel]');
  const preview = root.querySelector('[data-live-preview]');
  const status = root.querySelector('[data-live-status]');
  const devicesFieldset = root.querySelector('[data-live-devices]');
  const cameraDevice = root.querySelector('[data-live-camera-device]');
  const micDevice = root.querySelector('[data-live-mic-device]');
  const screenAudio = root.querySelector('[data-live-screen-audio]');
  if (!camera || !screen || !stopButton || !startButton || !cancelButton || !preview || !status ||
      !devicesFieldset || !cameraDevice || !micDevice || !screenAudio) return;

  const supported = !!(window.MediaRecorder && window.WebSocket &&
    window.AudioContext && HTMLCanvasElement.prototype.captureStream &&
    navigator.mediaDevices && navigator.mediaDevices.getUserMedia);
  root.hidden = false;
  if (!supported) {
    status.textContent = root.dataset.unsupported;
    camera.disabled = true;
    screen.disabled = true;
    return;
  }
  if (!navigator.mediaDevices.getDisplayMedia) screen.disabled = true;
  devicesFieldset.hidden = false;

  async function refreshDevices() {
    if (!navigator.mediaDevices.enumerateDevices) return;
    let devices;
    try { devices = await navigator.mediaDevices.enumerateDevices(); }
    catch { return; }
    function populate(select, kind, unknown) {
      const selected = select.value;
      const defaultOption = select.options[0].cloneNode(true);
      select.replaceChildren(defaultOption);
      let count = 0;
      for (const device of devices) {
        if (device.kind !== kind || !device.deviceId) continue;
        count += 1;
        select.add(new Option(device.label || `${unknown} ${count}`, device.deviceId));
      }
      if ([...select.options].some(option => option.value === selected)) select.value = selected;
    }
    populate(cameraDevice, 'videoinput', root.dataset.cameraUnknown);
    populate(micDevice, 'audioinput', root.dataset.micUnknown);
  }
  refreshDevices();
  navigator.mediaDevices.addEventListener?.('devicechange', refreshDevices);

  let stream = null;
  let sourceStream = null;
  let sourceVideo = null;
  let sourceAudio = null;
  let audioContext = null;
  let canvas = null;
  let frameTimer = null;
  let activeKind = null;
  let switching = false;
  let socket = null;
  let recorder = null;
  let sendChain = Promise.resolve();
  let stopping = false;
  let stopAck = null;

  function tell(message) { status.textContent = message; }
  function onAir() {
    return `${root.dataset.onAir} ${activeKind === 'screen'
      ? root.dataset.currentScreen : root.dataset.currentCamera}`;
  }
  function buttons(active, busy = false) {
    camera.textContent = !active ? camera.defaultText : activeKind === 'camera'
      ? root.dataset.liveCameraLabel : root.dataset.switchCamera;
    screen.textContent = !active ? screen.defaultText : activeKind === 'screen'
      ? root.dataset.liveScreenLabel : root.dataset.switchScreen;
    camera.disabled = busy || (active && activeKind === 'camera');
    screen.disabled = busy || (active && activeKind === 'screen') || !navigator.mediaDevices.getDisplayMedia;
    stopButton.hidden = !active;
    startButton.hidden = active || !stream;
    cancelButton.hidden = active || !stream;
    startButton.disabled = busy;
    cancelButton.disabled = busy;
  }
  camera.defaultText = camera.textContent;
  screen.defaultText = screen.textContent;
  function releaseTracks() {
    if (stream) stream.getTracks().forEach(track => track.stop());
    if (sourceStream) sourceStream.getTracks().forEach(track => track.stop());
    if (sourceVideo) sourceVideo.srcObject = null;
    if (frameTimer) clearInterval(frameTimer);
    if (audioContext) audioContext.close().catch(() => {});
    stream = null;
    sourceStream = null;
    sourceVideo = null;
    sourceAudio = null;
    audioContext = null;
    canvas = null;
    frameTimer = null;
    activeKind = null;
    switching = false;
    preview.srcObject = null;
    preview.hidden = true;
  }
  async function capture(kind) {
    if (kind === 'screen') return navigator.mediaDevices.getDisplayMedia({
      video: { frameRate: 30 }, audio: screenAudio.checked,
    });
    return navigator.mediaDevices.getUserMedia({
      video: {
        width: { ideal: 1280 }, height: { ideal: 720 }, frameRate: { ideal: 30 },
        ...(cameraDevice.value ? { deviceId: { exact: cameraDevice.value } } : {}),
      },
      audio: micDevice.value ? { deviceId: { exact: micDevice.value } } : true,
    });
  }
  async function playableVideo(captured) {
    if (!captured.getVideoTracks().length) throw new Error(root.dataset.deviceUnavailable);
    const video = document.createElement('video');
    video.muted = true;
    video.playsInline = true;
    video.srcObject = captured;
    try { await video.play(); }
    catch (error) { video.srcObject = null; throw error; }
    return video;
  }
  function audioInput(captured) {
    const tracks = captured.getAudioTracks();
    if (!tracks.length) return null;
    const input = audioContext.createMediaStreamSource(new MediaStream(tracks));
    input.connect(audioContext.destinationStream);
    return input;
  }
  function watchSource(captured) {
    captured.getVideoTracks().forEach(track => track.addEventListener('ended', () => {
      if (sourceStream === captured) stop();
    }, { once: true }));
  }
  async function setupOutput(captured, kind) {
    const video = await playableVideo(captured);
    const settings = captured.getVideoTracks()[0].getSettings();
    const width = settings.width || video.videoWidth || 1280;
    const height = settings.height || video.videoHeight || 720;
    const scale = Math.min(1, 1280 / width, 720 / height);
    canvas = document.createElement('canvas');
    canvas.width = Math.max(1, Math.round(width * scale));
    canvas.height = Math.max(1, Math.round(height * scale));
    const context = canvas.getContext('2d', { alpha: false });
    if (!context) throw new Error(root.dataset.unsupported);
    audioContext = new AudioContext();
    // A constant silent input keeps an audio track and codec across camera ↔
    // silent-screen switches; the active source is the only audible input.
    audioContext.destinationStream = audioContext.createMediaStreamDestination();
    const silence = audioContext.createConstantSource();
    const mute = audioContext.createGain();
    mute.gain.value = 0;
    silence.connect(mute).connect(audioContext.destinationStream);
    silence.start();
    let audioDeadline;
    try {
      await Promise.race([
        audioContext.resume(),
        new Promise((_, reject) => {
          audioDeadline = setTimeout(() => reject(new Error(root.dataset.audioUnavailable)), 5000);
        }),
      ]);
    } finally { clearTimeout(audioDeadline); }
    sourceAudio = audioInput(captured);
    sourceVideo = video;
    sourceStream = captured;
    activeKind = kind;
    watchSource(captured);
    const draw = () => {
      if (!sourceVideo || sourceVideo.readyState < HTMLMediaElement.HAVE_CURRENT_DATA) return;
      const sourceWidth = sourceVideo.videoWidth || canvas.width;
      const sourceHeight = sourceVideo.videoHeight || canvas.height;
      const scale = Math.min(canvas.width / sourceWidth, canvas.height / sourceHeight);
      const width = sourceWidth * scale;
      const height = sourceHeight * scale;
      context.fillStyle = '#000';
      context.fillRect(0, 0, canvas.width, canvas.height);
      context.drawImage(sourceVideo, (canvas.width - width) / 2,
        (canvas.height - height) / 2, width, height);
    };
    draw();
    frameTimer = setInterval(draw, 1000 / 30);
    stream = new MediaStream([
      ...canvas.captureStream(30).getVideoTracks(),
      ...audioContext.destinationStream.stream.getAudioTracks(),
    ]);
  }
  async function switchSource(kind) {
    if (!recorder || stopping || switching || activeKind === kind) return;
    switching = true;
    buttons(true, true);
    tell(root.dataset.switching);
    let captured = null;
    let video = null;
    try {
      captured = await capture(kind);
      if (stopping) return;
      video = await playableVideo(captured);
      if (stopping) return;
      const audio = audioInput(captured);
      const oldStream = sourceStream;
      const oldVideo = sourceVideo;
      if (sourceAudio) sourceAudio.disconnect();
      sourceAudio = audio;
      sourceVideo = video;
      sourceStream = captured;
      activeKind = kind;
      watchSource(captured);
      oldStream.getTracks().forEach(track => track.stop());
      oldVideo.srcObject = null;
      captured = null;
      video = null;
      refreshDevices();
      tell(onAir());
    } catch (error) {
      tell(`${root.dataset.switchFailed} ${captureError(error)}`);
    } finally {
      if (captured) captured.getTracks().forEach(track => track.stop());
      if (video) video.srcObject = null;
      switching = false;
      if (!stopping) {
        buttons(true);
        (activeKind === kind ? stopButton : kind === 'screen' ? screen : camera).focus();
      }
    }
  }
  function chosenMime(captured) {
    // The output normally has a stable audio track, including for a silent
    // screen. Retain a video-only fallback for a browser without that track.
    const hasAudio = captured.getAudioTracks().length > 0;
    const candidates = hasAudio ? [
      'video/webm;codecs=vp8,opus', 'video/webm;codecs=vp8', 'video/webm',
      'video/mp4;codecs=avc1.42E01E,mp4a.40.2', 'video/mp4',
    ] : [
      'video/webm;codecs=vp8', 'video/webm',
      'video/mp4;codecs=avc1.42E01E', 'video/mp4',
    ];
    return candidates.find(type => MediaRecorder.isTypeSupported(type));
  }

  function connect(mime) {
    return new Promise((resolve, reject) => {
      const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
      const url = `${protocol}//${location.host}/web/videos/live/${root.dataset.statusId}/publish`;
      const connection = new WebSocket(url);
      socket = connection;
      let settled = false;
      let ready = false;
      const timeout = setTimeout(() => fail(new Error(root.dataset.startTimeout)), 15000);
      function fail(error) {
        if (settled) return;
        settled = true;
        clearTimeout(timeout);
        connection.close();
        reject(error);
      }
      connection.onopen = () => {
        if (!settled) connection.send(JSON.stringify({ csrf: root.dataset.csrf, mime }));
      };
      connection.onmessage = event => {
        let result;
        try { result = JSON.parse(event.data); } catch { result = {}; }
        if (result.ready) {
          if (settled) return;
          settled = true;
          ready = true;
          clearTimeout(timeout);
          resolve();
        }
        else if (result.stopped && stopAck) stopAck(true);
        else if (result.error) {
          if (!settled) fail(new Error(result.error));
          else if (ready && !stopping) { tell(result.error); stop(); }
        }
      };
      connection.onerror = () => {
        if (!settled) fail(new Error(root.dataset.startError));
        else if (ready && !stopping) { tell(root.dataset.connectionError); stop(); }
      };
      connection.onclose = () => {
        if (stopAck) stopAck(false);
        if (!settled) fail(new Error(root.dataset.startError));
        else if (ready && !stopping) { tell(root.dataset.connectionError); stop(); }
      };
    });
  }

  function captureError(error) {
    switch (error?.name) {
      case 'NotAllowedError':
      case 'SecurityError': return root.dataset.permissionDenied;
      case 'NotFoundError':
      case 'OverconstrainedError': return root.dataset.deviceUnavailable;
      case 'NotReadableError': return root.dataset.deviceBusy;
      case 'AbortError': return root.dataset.captureCancelled;
      default: return error?.message || root.dataset.startError;
    }
  }

  async function prepare(kind) {
    releaseTracks();
    buttons(false, true);
    stopping = false;
    tell(root.dataset.requesting);
    let captured = null;
    try {
      captured = await capture(kind);
      if (stopping) throw new Error(root.dataset.captureCancelled);
      refreshDevices();
      await setupOutput(captured, kind);
      captured = null;
      if (stopping) { releaseTracks(); return; }
      if (!chosenMime(stream)) throw new Error(root.dataset.unsupported);
      preview.srcObject = stream;
      preview.hidden = false;
      preview.play().catch(() => {});
      buttons(false);
      tell(root.dataset.previewReady);
      startButton.focus();
    } catch (error) {
      if (captured) captured.getTracks().forEach(track => track.stop());
      releaseTracks(); buttons(false); tell(captureError(error));
    }
  }

  async function start() {
    if (!stream || recorder) return;
    buttons(false, true);
    stopping = false;
    try {
      const mime = chosenMime(stream);
      await connect(mime);
      if (!socket || socket.readyState !== WebSocket.OPEN || stopping) {
        throw new Error(root.dataset.connectionError);
      }
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
        stopButton.disabled = false;
        if (status.textContent.startsWith(root.dataset.onAir) ||
            status.textContent === root.dataset.stopping ||
            status.textContent === root.dataset.switching ||
            status.textContent.startsWith(root.dataset.switchFailed)) {
          tell(acknowledged ? root.dataset.stopped : root.dataset.connectionError);
        }
      };
      recorder.start(1000);
      buttons(true);
      tell(onAir());
    } catch (error) {
      if (socket) socket.close();
      socket = null;
      recorder = null;
      releaseTracks();
      buttons(false);
      tell(error.message || root.dataset.startError);
    }
  }

  function stop() {
    if (stopping) return;
    stopping = true;
    stopButton.disabled = true;
    if (recorder && recorder.state !== 'inactive') {
      recorder.stop();
      // Final buffered recorder data still flushes over sendChain. Hardware
      // capture must not remain active while waiting for the server's ACK.
      buttons(true, true);
      releaseTracks();
      if (status.textContent.startsWith(root.dataset.onAir) ||
          status.textContent === root.dataset.switching ||
          status.textContent.startsWith(root.dataset.switchFailed)) tell(root.dataset.stopping);
    }
    else {
      if (socket) socket.close();
      socket = null;
      releaseTracks();
      buttons(false);
      stopButton.disabled = false;
      if (status.textContent === root.dataset.previewReady) tell(root.dataset.previewCancelled);
    }
  }

  camera.addEventListener('click', () => recorder ? switchSource('camera') : prepare('camera'));
  screen.addEventListener('click', () => recorder ? switchSource('screen') : prepare('screen'));
  startButton.addEventListener('click', start);
  cancelButton.addEventListener('click', () => { stop(); tell(root.dataset.previewCancelled); camera.focus(); });
  stopButton.addEventListener('click', stop);
  window.addEventListener('pagehide', stop);
})();
