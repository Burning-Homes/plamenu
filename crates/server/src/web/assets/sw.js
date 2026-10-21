"use strict";

/*
 * Plamenu's service worker: install resilience plus Web Push. The
 * server prepends PLAMENU_ASSET_VERSION so a CSS, JS, icon or worker change
 * produces different worker bytes and refreshes this small shell cache.
 *
 * The push payload is Mastodon's `Web::NotificationSerializer` shape:
 * { access_token, preferred_locale, notification_id, notification_type,
 *   icon, title, body }.
 */

const CACHE_NAME = `plamenu-shell-${PLAMENU_ASSET_VERSION}`;
const OFFLINE_URL = "/offline";
const SHELL_URLS = [
  OFFLINE_URL,
  "/assets/app.css",
  "/assets/app.js",
  "/pwa/icon-192.png",
  "/pwa/badge-96.png",
];
const SHELL_PATHS = new Set(SHELL_URLS);

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(CACHE_NAME)
      .then((cache) =>
        Promise.all(
          SHELL_URLS.map((url) =>
            cache.add(new Request(url, { cache: "reload" })),
          ),
        ),
      )
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    Promise.all([
      caches
        .keys()
        .then((names) =>
          Promise.all(
            names
              .filter((name) =>
                name.startsWith("plamenu-shell-") && name !== CACHE_NAME,
              )
              .map((name) => caches.delete(name)),
          ),
        ),
      self.registration.navigationPreload
        ? self.registration.navigationPreload.enable()
        : Promise.resolve(),
      self.clients.claim(),
    ]),
  );
});

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") return;

  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;

  if (request.mode === "navigate") {
    event.respondWith(
      (async () => {
        try {
          const preloaded = await event.preloadResponse;
          // An error response cannot fulfill navigation; let the offline path run.
          if (preloaded && preloaded.type !== "error") return preloaded;
          return await fetch(request);
        } catch {
          return (await caches.match(OFFLINE_URL)) || Response.error();
        }
      })(),
    );
    return;
  }

  // A worker from an older deployment must not substitute its cached assets
  // for the new page's versioned URLs while the replacement worker installs.
  if (SHELL_PATHS.has(url.pathname)) {
    const version = url.searchParams.get("v");
    if (version && version !== PLAMENU_ASSET_VERSION) {
      event.respondWith(fetch(request, { cache: "reload" }));
      return;
    }
    event.respondWith(
      caches.open(CACHE_NAME)
        .then((cache) => cache.match(url.pathname))
        .then((cached) => cached || fetch(request)),
    );
  }
});

function notificationTarget(value) {
  try {
    const url = new URL(value || "/notifications", self.location.origin);
    if (url.origin === self.location.origin) return url.pathname + url.search + url.hash;
  } catch { /* Invalid or external targets fall back to the inbox. */ }
  return "/notifications";
}

self.addEventListener("push", (event) => {
  if (!event.data) return;
  let payload;
  try {
    payload = event.data.json();
  } catch {
    return;
  }
  const title = payload.title || "Plamenu";
  const options = {
    body: payload.body || "",
    icon: payload.icon || "/pwa/icon-192.png",
    badge: "/pwa/badge-96.png",
    data: { url: notificationTarget(payload.url) },
  };
  // Collapse repeat deliveries of the same notification into one bubble.
  if (payload.notification_id) {
    options.tag = "plamenu-" + payload.notification_id;
  }
  event.waitUntil(self.registration.showNotification(title, options));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const url = notificationTarget(event.notification.data?.url);
  event.waitUntil(
    (async () => {
      const windows = await self.clients.matchAll({
        type: "window",
        includeUncontrolled: true,
      });
      // Refresh an existing target so new activity and its read state are
      // loaded. Leave other tabs alone: they may hold an unfinished draft.
      const target = new URL(url, self.location.origin).href;
      const onPage = windows.find((c) => c.url === target);
      if (onPage && "focus" in onPage) {
        if ("navigate" in onPage) await onPage.navigate(url);
        await onPage.focus();
        return;
      }
      if (self.clients.openWindow) {
        await self.clients.openWindow(url);
        return;
      }
      if (windows[0] && "focus" in windows[0]) await windows[0].focus();
    })(),
  );
});
