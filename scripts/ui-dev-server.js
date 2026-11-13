// Small local static server for the interface test bench (crates/app/ui-dev).
// Listens on 127.0.0.1 only. Usage: node scripts/ui-dev-server.js [port]
"use strict";
const http = require("http");
const fs = require("fs");
const path = require("path");

const root = path.resolve(__dirname, "..", "crates", "app");
const port = Number(process.argv[2]) || 5178;
const types = { ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".css": "text/css; charset=utf-8", ".png": "image/png", ".woff2": "font/woff2", ".svg": "image/svg+xml", ".ico": "image/x-icon", ".mp3": "audio/mpeg" };

http
  .createServer((req, res) => {
    let rel = decodeURIComponent(new URL(req.url, "http://x").pathname);
    if (rel === "/") { res.writeHead(302, { Location: "/ui-dev/index.html" }); return res.end(); }
    const file = path.resolve(root, "." + rel);
    if (!file.startsWith(root + path.sep)) { res.writeHead(403); return res.end(); }
    fs.readFile(file, (err, data) => {
      if (err) { res.writeHead(404); return res.end("introuvable"); }
      res.writeHead(200, { "Content-Type": types[path.extname(file)] || "application/octet-stream", "Cache-Control": "no-store" });
      res.end(data);
    });
  })
  .listen(port, "127.0.0.1", () => console.log(`Banc d'essai : http://127.0.0.1:${port}/ui-dev/index.html`));
