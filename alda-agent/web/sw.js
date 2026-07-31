"use strict";
const CACHE = "alda-agent-shell-v2";
const ALLOWLIST = ["/", "/app.js", "/client-state.js", "/app.css", "/manifest.webmanifest"];

self.addEventListener("install", event => {
  event.waitUntil(caches.open(CACHE).then(cache =>
    Promise.all(ALLOWLIST.map(path => fetch(path).then(response => {
      if (!response.ok || response.redirected) return undefined;
      return cache.put(path, response);
    })))));
});

self.addEventListener("activate", event => {
  event.waitUntil(caches.keys().then(keys =>
    Promise.all(keys.filter(key => key !== CACHE).map(key => caches.delete(key)))));
});

self.addEventListener("fetch", event => {
  const url = new URL(event.request.url);
  const allowed = event.request.method === "GET" &&
    url.origin === self.location.origin &&
    url.search === "" &&
    ALLOWLIST.includes(url.pathname);
  if (!allowed) return;
  event.respondWith(caches.match(event.request).then(hit =>
    hit || fetch(event.request).then(response => {
      if (!response.ok || response.redirected) return response;
      const copy = response.clone();
      caches.open(CACHE).then(cache => cache.put(event.request, copy));
      return response;
    })));
});
