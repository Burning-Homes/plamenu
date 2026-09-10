(() => {
  "use strict";
  const config = window.__plamenuWebxdc;
  if (!config || window.webxdc) return;

  const runtimeId = crypto.randomUUID();
  const pending = new Map();
  let listener = null;
  let activeRealtime = null;
  let realtimeGeneration = 0;
  let hostPort = null;
  let nextRequest = 1;
  let connect;
  const connected = new Promise((resolve) => { connect = resolve; });

  // The iframe sandbox independently blocks forms, popups, top-level
  // navigation and ambient origin access. These traps additionally remove
  // active network APIs. Package-local fetch remains usable on the dedicated
  // session host (including root-absolute bundle paths); the server makes that
  // host a package-only virtual host. CSP supplies another enforced boundary.
  const nativeFetch = window.fetch.bind(window);
  const localFetch = (input, init = {}) => {
    const raw = input instanceof Request ? input.url : String(input);
    const target = new URL(raw, window.location.href);
    const packageAsset = target.protocol === window.location.protocol &&
      target.host === window.location.host;
    // Three.js and other asset loaders fetch generated textures before decoding
    // them. These URLs read local bytes, without opening a network connection.
    const localAsset = target.protocol === "data:" ||
      (target.protocol === "blob:" && target.origin === new URL(window.location.href).origin);
    if (!packageAsset && !localAsset) {
      return Promise.reject(new Error("Internet access is disabled in Webxdc apps"));
    }
    return nativeFetch(input, { ...init, credentials: "omit", redirect: "error" });
  };
  Object.defineProperty(window, "fetch", { value: localFetch, configurable: false, writable: false });
  for (const name of ["XMLHttpRequest", "WebSocket", "EventSource", "WebTransport", "RTCPeerConnection"]) {
    if (name in window) {
      Object.defineProperty(window, name, {
        value: class { constructor() { throw new Error("Internet access is disabled in Webxdc apps"); } },
        configurable: false,
        writable: false,
      });
    }
  }
  try {
    Object.defineProperty(navigator, "sendBeacon", { value: () => false, configurable: false });
  } catch (_) { /* CSP still blocks it on engines with a fixed prototype method. */ }
  Object.defineProperty(window, "open", { value: () => null, configurable: false, writable: false });

  function receive(message) {
    if (!message || message.channel !== "plamenu-webxdc" || message.session !== config.session) return;
    if (message.op === "realtime") {
      if (activeRealtime?.generation === message.generation && activeRealtime.listener &&
          message.value instanceof Uint8Array) activeRealtime.listener(message.value);
      return;
    }
    if (message.op === "update" && listener) {
      listener(message.value);
      return;
    }
    const waiting = pending.get(message.requestId);
    if (!waiting) return;
    pending.delete(message.requestId);
    if (message.error) waiting.reject(new Error(message.error));
    else waiting.resolve(message.value);
  }

  // Each session has a dedicated cross-origin host, so browser storage works
  // without becoming visible to another Webxdc session. The trusted parent
  // bootstraps a dedicated MessageChannel; this side authenticates its real
  // origin and window before accepting the transferred port.
  function acceptChannel(event) {
    const message = event.data;
    if (event.origin !== config.hostOrigin || event.source !== window.parent ||
        !message || message.channel !== "plamenu-webxdc-init" ||
        message.session !== config.session || message.runtimeId !== runtimeId || !event.ports[0]) return;
    window.removeEventListener("message", acceptChannel);
    clearInterval(readyTimer);
    const port = event.ports[0];
    port.onmessage = (portEvent) => receive(portEvent.data);
    port.start();
    hostPort = port;
    connect(port);
  }
  window.addEventListener("message", acceptChannel);
  const announceReady = () => window.parent.postMessage(
    { channel: "plamenu-webxdc-ready", session: config.session, runtimeId },
    config.hostOrigin,
  );
  // The package can execute before the parent page's host script has attached
  // its message listener (especially on a cached reload). Repeat the ready
  // signal until the authenticated MessageChannel arrives, so update replay
  // cannot be lost to that startup race.
  const readyTimer = setInterval(announceReady, 250);
  announceReady();

  function request(op, value) {
    const requestId = nextRequest++;
    return connected.then((port) => new Promise((resolve, reject) => {
      pending.set(requestId, { resolve, reject });
      port.postMessage({ channel: "plamenu-webxdc", session: config.session, requestId, op, value });
    }));
  }

  window.webxdc = Object.freeze({
    selfAddr: config.selfAddr,
    selfName: config.selfName,
    sendUpdateInterval: config.sendUpdateInterval,
    sendUpdateMaxSize: config.sendUpdateMaxSize,
    sendUpdate(update, description) {
      return request("sendUpdate", { update, description: description || "" });
    },
    setUpdateListener(callback, serial = 0) {
      if (typeof callback !== "function") throw new TypeError("callback must be a function");
      listener = callback;
      return request("listen", { serial: Number.isSafeInteger(serial) && serial >= 0 ? serial : 0 });
    },
    joinRealtimeChannel() {
      if (activeRealtime) throw new Error("The realtime channel is already joined");
      const channel = { generation: ++realtimeGeneration, listener: null };
      activeRealtime = channel;
      const check = () => {
        if (activeRealtime !== channel) throw new Error("The realtime channel has been left");
      };
      const post = (op, value) => hostPort?.postMessage({
        channel: "plamenu-webxdc", session: config.session,
        op, generation: channel.generation, value,
      });
      connected.then(() => {
        if (activeRealtime === channel) post("realtimeJoin");
      });
      return Object.freeze({
        setListener(callback) {
          check();
          if (typeof callback !== "function") throw new TypeError("callback must be a function");
          channel.listener = callback;
        },
        send(data) {
          check();
          if (!(data instanceof Uint8Array)) throw new TypeError("data must be a Uint8Array");
          if (data.byteLength > 128000) throw new RangeError("realtime data exceeds 128000 bytes");
          // No buffer while the trusted host is disconnected.
          post("realtimeSend", data);
        },
        leave() {
          check();
          activeRealtime = null;
          channel.listener = null;
          post("realtimeLeave");
        },
      });
    },
  });
})();
