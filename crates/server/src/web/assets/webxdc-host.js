(() => {
  "use strict";
  const frame = document.querySelector("iframe[data-webxdc-session]");
  if (!frame) return;
  const fullscreenButton = document.querySelector("[data-webxdc-fullscreen]");
  if (fullscreenButton && document.fullscreenEnabled) {
    const error = document.querySelector(".webxdc-fullscreen-error");
    fullscreenButton.hidden = false;
    document.addEventListener("fullscreenchange", () => {
      const active = document.fullscreenElement === frame;
      fullscreenButton.textContent = active ? "Exit fullscreen" : "Fullscreen";
      fullscreenButton.setAttribute("aria-pressed", String(active));
    });
    fullscreenButton.addEventListener("click", async () => {
      error.hidden = true;
      try {
        if (document.fullscreenElement === frame) await document.exitFullscreen();
        else await frame.requestFullscreen();
      } catch (_error) {
        error.hidden = false;
      }
    });
  }
  // Plamenu IDs are 64-bit snowflakes and can exceed JavaScript's safe integer
  // range. Treat the browser-side routing token as an opaque string.
  const session = frame.dataset.webxdcSession;
  const csrf = frame.dataset.webxdcCsrf;
  const runtimeOrigin = frame.dataset.webxdcOrigin;
  let after = 0;
  let listening = false;
  let polling = false;
  let port = null;
  let runtimeId = null;
  let realtime = null;
  let stopped = false;
  let realtimeGeneration = null;
  let reconnectTimer = null;

  function stopRuntime() {
    stopped = true;
    listening = false;
    closeRealtime();
    port?.close();
    port = null;
    frame.remove();
    const notice = document.querySelector(".webxdc-player__notice");
    if (notice) {
      notice.hidden = false;
      notice.textContent = "This session has ended or access is no longer available. Open session details to continue.";
      notice.setAttribute("role", "status");
    }
  }

  function closeRealtime() {
    clearTimeout(reconnectTimer);
    realtimeGeneration = null;
    const socket = realtime;
    realtime = null;
    socket?.close();
  }

  function openRealtime(generation) {
    const socket = new WebSocket(`wss://${location.host}/webxdc/${session}/realtime`);
    socket.binaryType = "arraybuffer";
    realtime = socket;
    socket.onmessage = (event) => {
      if (socket !== realtime || generation !== realtimeGeneration ||
          !(event.data instanceof ArrayBuffer) || event.data.byteLength > 128000) return;
      port?.postMessage({ channel: "plamenu-webxdc", session, op: "realtime",
        generation, value: new Uint8Array(event.data) });
    };
    socket.onclose = (event) => {
      if (socket !== realtime) return;
      realtime = null;
      if (event.code === 1000) { stopRuntime(); return; }
      // Rejoin after transport loss, never retain or resend data. A normal
      // server close (revoked membership/ended session) stops reconnecting.
      if (event.code !== 1000 && realtimeGeneration === generation) {
        reconnectTimer = setTimeout(() => openRealtime(generation), 1500);
      }
    };
  }

  function reply(requestId, value, error) {
    port?.postMessage({ channel: "plamenu-webxdc", session, requestId, value, error });
  }

  async function updates() {
    if (!listening || polling || !port) return;
    polling = true;
    try {
      const response = await fetch(`/webxdc/${session}/updates?after=${after}`, {
        credentials: "same-origin",
        headers: { Accept: "application/json" },
      });
      if ([403, 404, 410].includes(response.status)) { stopRuntime(); return; }
      if (!response.ok) throw new Error(`update sync failed (${response.status})`);
      const result = await response.json();
      if (result.ended) { stopRuntime(); return; }
      for (const item of result.updates) {
        after = Math.max(after, item.serial);
        port.postMessage({
          channel: "plamenu-webxdc",
          session,
          op: "update",
          value: { ...item.webxdcUpdate, serial: item.serial, max_serial: result.maxSerial },
        });
      }
    } catch (error) {
      console.error("Webxdc durable update synchronization failed", error);
    } finally {
      polling = false;
    }
  }

  async function receive(message) {
    if (!message || message.channel !== "plamenu-webxdc" || message.session !== session) return;
    try {
      if (message.op === "realtimeJoin") {
        if (!Number.isSafeInteger(message.generation) || message.generation < 1) return;
        closeRealtime();
        realtimeGeneration = message.generation;
        openRealtime(message.generation);
      } else if (message.op === "realtimeLeave") {
        if (message.generation === realtimeGeneration) closeRealtime();
      } else if (message.op === "realtimeSend") {
        if (message.generation !== realtimeGeneration || !(message.value instanceof Uint8Array) ||
            message.value.byteLength > 128000) return;
        if (realtime?.readyState === WebSocket.OPEN && realtime.bufferedAmount === 0) {
          realtime.send(message.value);
        }
      } else if (message.op === "listen") {
        after = Math.max(0, Number(message.value?.serial) || 0);
        listening = true;
        await updates();
        reply(message.requestId, true);
      } else if (message.op === "sendUpdate") {
        const response = await fetch(`/web/webxdc/${session}/updates`, {
          method: "POST",
          credentials: "same-origin",
          headers: { "Content-Type": "application/json", "X-CSRF-Token": csrf },
          body: JSON.stringify(message.value?.update),
        });
        const result = await response.json().catch(() => ({}));
        if (!response.ok) throw new Error(result.error || `sendUpdate failed (${response.status})`);
        reply(message.requestId, result);
        await updates();
      }
    } catch (error) {
      reply(message.requestId, null, error instanceof Error ? error.message : String(error));
    }
  }

  function connect() {
    if (port || stopped) return;
    const channel = new MessageChannel();
    port = channel.port1;
    port.onmessage = (event) => receive(event.data);
    port.start();
    frame.contentWindow.postMessage(
      { channel: "plamenu-webxdc-init", session, runtimeId },
      runtimeOrigin,
      [channel.port2],
    );
  }

  window.addEventListener("message", (event) => {
    const message = event.data;
    if (event.source !== frame.contentWindow || event.origin !== runtimeOrigin ||
        !message || message.channel !== "plamenu-webxdc-ready" ||
        message.session !== session || typeof message.runtimeId !== "string" ||
        message.runtimeId.length > 128) return;
    if (runtimeId !== message.runtimeId) {
      closeRealtime();
      port?.close();
      port = null;
      listening = false;
      runtimeId = message.runtimeId;
    }
    connect();
  });
  window.addEventListener("pagehide", closeRealtime);
  setInterval(updates, 1500);
})();
