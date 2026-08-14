import { readFileSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { basename, resolve } from "node:path";

const [fileArgument, readyArgument] = process.argv.slice(2);
if (!fileArgument || !readyArgument) {
  throw new Error("usage: bootstrap-http-server.mjs <file> <ready-file>");
}

const file = resolve(fileArgument);
const readyFile = resolve(readyArgument);
const name = basename(file);
const body = readFileSync(file);
const latestPath = `/latest/download/${name}`;
const assetPath = `/asset/${name}`;

const server = createServer((request, response) => {
  if (request.url === latestPath) {
    response.statusCode = 302;
    response.setHeader("Location", assetPath);
    response.setHeader("Content-Length", "0");
    response.end();
    return;
  }
  if (request.url === assetPath) {
    response.statusCode = 200;
    response.setHeader("Content-Type", "application/octet-stream");
    response.setHeader("Content-Length", String(body.length));
    response.end(body, () => server.close());
    return;
  }
  response.statusCode = 404;
  response.end("not found");
});

const timer = setTimeout(() => {
  server.close();
  process.exitCode = 1;
}, 30_000);
timer.unref();

server.listen(0, "127.0.0.1", () => {
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("bootstrap server did not bind a TCP port");
  }
  writeFileSync(readyFile, `http://127.0.0.1:${address.port}`, "utf8");
});
