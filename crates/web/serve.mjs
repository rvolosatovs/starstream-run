// Zero-dependency static file server for the built web/ directory.
// Run with `npm run serve` (or `node serve.mjs`). A plain `file://` open won't
// work: the page is an ES module and fetches the ~11 MB .wasm runtime, both of
// which need real HTTP responses with correct MIME types.

import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, join, normalize, sep } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("./web", import.meta.url));
const port = Number(process.env.PORT) || 8080;

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".wasm": "application/wasm",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".map": "application/json; charset=utf-8",
};

const server = createServer(async (req, res) => {
  try {
    const { pathname } = new URL(req.url, "http://localhost");
    let rel = decodeURIComponent(pathname);
    if (rel.endsWith("/")) rel += "index.html";

    const filePath = normalize(join(root, rel));
    // Reject anything that escapes the served root.
    if (filePath !== root && !filePath.startsWith(root + sep)) {
      res.writeHead(403).end("Forbidden");
      return;
    }

    const body = await readFile(filePath);
    res.writeHead(200, {
      "content-type": MIME[extname(filePath)] ?? "application/octet-stream",
      // Dev server: never cache. Browsers otherwise heuristically cache the
      // ~11 MB .wasm runtime and the wasm-bindgen JS glue, so after a rebuild
      // a reload would serve the stale module (new HTML, old behaviour).
      "cache-control": "no-store, no-cache, must-revalidate",
    });
    res.end(body);
  } catch (err) {
    if (err.code === "ENOENT") {
      res.writeHead(404).end("Not found");
    } else {
      res.writeHead(500).end("Internal server error");
    }
  }
});

server.listen(port, () => {
  console.log(`serving ${root} at http://localhost:${port}`);
});
