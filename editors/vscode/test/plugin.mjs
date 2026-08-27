// The Studio plugin listener, driven over real HTTP.
//
// Real sockets rather than a mocked `http`: what is being asserted is a wire
// format an unmodified Studio plugin has to work against, and a mock would
// only ever agree with whatever this file believes that format to be. The size
// cap in particular is enforced mid-stream, which nothing but a real request
// exercises.
//
// This imports the compiled `out/plugin.js`, so `npm run compile` has to have
// run. That is why the extension's `test` script builds first.

import http from "node:http";
import net from "node:net";
import { bytes, startPluginServer } from "../out/plugin.js";

let failures = 0;
let checks = 0;

function check(label, ok, detail) {
  checks++;
  if (!ok) {
    failures++;
    console.log(`  FAIL  ${label}`);
    if (detail !== undefined) {
      console.log(`        ${detail}`);
    }
  }
}

function equal(label, actual, expected) {
  check(label, actual === expected, `expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
}

/// A port nothing is listening on, found by briefly listening on one.
function freePort() {
  return new Promise((resolve, reject) => {
    const probe = net.createServer();
    probe.on("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const { port } = probe.address();
      probe.close(() => resolve(port));
    });
  });
}

function post(port, path, body) {
  return new Promise((resolve, reject) => {
    const payload = body === undefined ? "" : body;
    const request = http.request(
      {
        host: "127.0.0.1",
        port,
        path,
        method: "POST",
        headers: { "content-type": "application/json", "content-length": Buffer.byteLength(payload) },
      },
      (response) => {
        let text = "";
        response.setEncoding("utf8");
        response.on("data", (chunk) => (text += chunk));
        response.on("end", () => resolve({ status: response.statusCode, text }));
      },
    );
    request.on("error", reject);
    request.end(payload);
  });
}

function get(port, path) {
  return new Promise((resolve, reject) => {
    http
      .get({ host: "127.0.0.1", port, path }, (response) => {
        let text = "";
        response.setEncoding("utf8");
        response.on("data", (chunk) => (text += chunk));
        response.on("end", () => resolve({ status: response.statusCode, text }));
      })
      .on("error", reject);
  });
}

/// A listener plus the notifications and log lines it produced.
async function listening(overrides = {}) {
  const port = await freePort();
  const sent = [];
  const logged = [];

  const server = startPluginServer({
    port,
    limit: 1024,
    forwardTo: () => null,
    notify: (method, params) => sent.push({ method, params }),
    listFiles: async () => ["/p/src/App.luaux"],
    log: (message) => logged.push(message),
    ...overrides,
  });

  // `listen` is async; the log line is emitted from its callback.
  await new Promise((resolve) => setTimeout(resolve, 60));
  return { port, sent, logged, server };
}

const TREE = { name: "Game", className: "DataModel", children: [] };

// --- the happy path --------------------------------------------------------

{
  const { port, sent, server } = await listening();

  const response = await post(port, "/full", JSON.stringify({ tree: TREE }));
  equal("POST /full answers 200", response.status, 200);
  equal("POST /full sends $/plugin/full", sent[0]?.method, "$/plugin/full");
  check(
    "POST /full forwards the tree itself, not the envelope",
    JSON.stringify(sent[0]?.params) === JSON.stringify(TREE),
    JSON.stringify(sent[0]?.params),
  );

  const cleared = await post(port, "/clear");
  equal("POST /clear answers 200", cleared.status, 200);
  equal("POST /clear sends $/plugin/clear", sent[1]?.method, "$/plugin/clear");

  const files = await get(port, "/get-file-paths");
  equal("GET /get-file-paths answers 200", files.status, 200);
  check(
    "GET /get-file-paths answers with the file list",
    JSON.parse(files.text).files[0] === "/p/src/App.luaux",
    files.text,
  );

  server.close();
}

// --- what the plugin does wrong --------------------------------------------

{
  const { port, sent, server } = await listening();

  const missing = await post(port, "/full", JSON.stringify({ notATree: 1 }));
  equal("a body with no tree is refused", missing.status, 400);

  const malformed = await post(port, "/full", "{{{");
  equal("a body that is not JSON is refused", malformed.status, 400);

  const unknown = await post(port, "/nope", "{}");
  equal("an unknown route is 404", unknown.status, 404);

  const wrongMethod = await get(port, "/full");
  equal("GET on /full is 404", wrongMethod.status, 404);

  check("nothing malformed reached the server", sent.length === 0, JSON.stringify(sent));
  server.close();
}

// --- the size cap ----------------------------------------------------------
//
// A DataModel from a large place file is genuinely tens of megabytes, so this
// is the ordinary failure rather than an exotic one — and the answer has to say
// which setting moves it.

{
  const { port, sent, server } = await listening({ limit: 256 });

  const huge = JSON.stringify({ tree: { name: "x".repeat(4096) } });
  const response = await post(port, "/full", huge);

  equal("a body over the cap is refused with 413", response.status, 413);
  check(
    "the refusal names the setting that raises the cap",
    response.text.includes("luaux.plugin.maximumRequestBodySize"),
    response.text,
  );
  check("nothing over the cap reached the server", sent.length === 0, JSON.stringify(sent));

  server.close();
}

// --- fanning out to the other extension ------------------------------------
//
// The Studio plugin can only be pointed at one port, so whichever extension
// holds it has to re-post to the other or the neighbour gets nothing.

{
  const downstreamPort = await freePort();
  const received = [];

  const downstream = http.createServer((request, response) => {
    let text = "";
    request.on("data", (chunk) => (text += chunk));
    request.on("end", () => {
      received.push({ path: request.url, text });
      response.writeHead(200);
      response.end();
    });
  });
  await new Promise((resolve) => downstream.listen(downstreamPort, "127.0.0.1", resolve));

  const { port, server } = await listening({ forwardTo: () => downstreamPort });

  await post(port, "/full", JSON.stringify({ tree: TREE }));
  await new Promise((resolve) => setTimeout(resolve, 120));

  equal("the DataModel is re-posted downstream", received[0]?.path, "/full");
  check(
    "downstream receives the body verbatim",
    JSON.parse(received[0]?.text ?? "{}").tree?.name === "Game",
    received[0]?.text,
  );

  server.close();
  downstream.close();
}

/// An unreachable neighbour must not fail the request it is being copied on:
/// the forward is a courtesy, and our own DataModel matters more than theirs.
{
  const dead = await freePort();
  const { port, sent, logged, server } = await listening({ forwardTo: () => dead });

  const response = await post(port, "/full", JSON.stringify({ tree: TREE }));
  await new Promise((resolve) => setTimeout(resolve, 200));

  equal("a dead downstream still answers 200", response.status, 200);
  equal("a dead downstream still reaches our own server", sent[0]?.method, "$/plugin/full");
  check(
    "the failed forward is reported",
    logged.some((line) => line.includes("could not forward")),
    JSON.stringify(logged),
  );

  // Said once per reason, not once per DataModel. `Set.add` returns the set
  // rather than a boolean, and getting that wrong logs on every single one.
  const before = logged.filter((line) => line.includes("could not forward")).length;
  await post(port, "/full", JSON.stringify({ tree: TREE }));
  await post(port, "/full", JSON.stringify({ tree: TREE }));
  await new Promise((resolve) => setTimeout(resolve, 200));
  const after = logged.filter((line) => line.includes("could not forward")).length;

  equal("the failed forward is reported once, not per request", after, before);

  server.close();
}

// --- a port somebody else has ----------------------------------------------
//
// The luau-lsp extension with its own plugin server on. Both cannot hold it,
// and this one must degrade rather than throw: the .luaux features that do not
// need a DataModel all keep working.

{
  const taken = await freePort();
  const squatter = http.createServer(() => {});
  await new Promise((resolve) => squatter.listen(taken, "127.0.0.1", resolve));

  const logged = [];
  const server = startPluginServer({
    port: taken,
    limit: 1024,
    forwardTo: () => null,
    notify: () => {},
    listFiles: async () => [],
    log: (message) => logged.push(message),
  });

  await new Promise((resolve) => setTimeout(resolve, 120));

  check(
    "a port already in use is explained rather than thrown",
    logged.some((line) => line.includes("already in use")),
    JSON.stringify(logged),
  );
  check(
    "and the explanation says how to fix it",
    logged.some((line) => line.includes("luaux.plugin.forwardTo")),
    JSON.stringify(logged),
  );

  server.close();
  squatter.close();
}

// --- the size setting ------------------------------------------------------

equal("bytes parses megabytes", bytes("3mb"), 3 * 1024 * 1024);
equal("bytes parses kilobytes", bytes("512kb"), 512 * 1024);
equal("bytes parses bare numbers as bytes", bytes("1024"), 1024);
equal("bytes is case and space insensitive", bytes("  2 MB "), 2 * 1024 * 1024);
equal("bytes falls back when unset", bytes(undefined), 3 * 1024 * 1024);
// A typo should cost the setting, not every DataModel the plugin sends.
equal("bytes falls back on nonsense", bytes("banana"), 3 * 1024 * 1024);
equal("bytes falls back on zero", bytes("0mb"), 3 * 1024 * 1024);

console.log(`\n${checks} plugin check(s), ${failures} failure(s)`);
process.exit(failures > 0 ? 1 : 0);
