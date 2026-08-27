// The Roblox Studio plugin's end of the wire.
//
// luau-lsp's companion Studio plugin does not talk LSP and does not know what a
// language server is. It POSTs the DataModel to a local HTTP port, and the
// *editor extension* — not the server binary — is what listens and relays it as
// a `$/plugin/full` notification. So a proxy that forwards settings, flags and
// definitions still gets no DataModel: nobody is holding the port.
//
// This is that listener, speaking the same wire format as luau-lsp's own so an
// unmodified Studio plugin works against it with nothing reconfigured:
//
//   POST /full            { tree }  ->  $/plugin/full
//   POST /clear                     ->  $/plugin/clear
//   GET  /get-file-paths            ->  { files: [...] }
//
// Written on `node:http` rather than express, which is what luau-lsp uses. The
// surface is three routes and the extension otherwise has no runtime
// dependencies at all; adding a framework and its tree to a `.vsix` to serve
// three routes is not a trade worth making.
//
// Nothing here imports `vscode`, and that is deliberate rather than tidy: a
// module that reaches for the editor API can only run inside an editor, and
// this one is a wire format with edge cases — a size cap, a fan-out that must
// not fail the request it is copying — which is exactly the kind of thing that
// should be tested without one. The editor glue lives in `extension.ts`.
//
// What this buys is instance types — `game.ReplicatedStorage.Thing` resolving
// in a project with no rojo sourcemap. It is **not** what makes a `.luaux`
// requiring a `.luaux` work: the tree carries instances, not file paths, and
// that half is the server's `workspace` module.

import * as http from "node:http";

export interface PluginOptions {
  port: number;
  /// Largest body to accept, in bytes.
  limit: number;
  /// Where to re-post what arrives, read fresh so a settings change applies
  /// without a restart. `null` for nowhere.
  forwardTo: () => number | null;
  /// Hands a `$/plugin/*` notification to the language server.
  notify: (method: string, params?: unknown) => void;
  /// Answers `/get-file-paths`.
  listFiles: () => Promise<string[]>;
  log: (message: string) => void;
}

export interface PluginServer {
  close: () => void;
}

/// Only one process can hold a port. The luau-lsp extension defaults its own
/// plugin server to *off*, so 3667 is normally free and taking it is the whole
/// setup — but "normally" is not "always", and a user with both switched on
/// must not get a stack trace out of it.
export function startPluginServer(options: PluginOptions): PluginServer {
  const complained = new Set<string>();

  const server = http.createServer((request, response) => {
    handle(request, response, options, complained).catch((error) => {
      options.log(`studio plugin: ${message(error)}`);
      if (!response.headersSent) {
        response.writeHead(500);
      }
      response.end();
    });
  });

  server.on("error", (error: NodeJS.ErrnoException) => {
    if (error.code === "EADDRINUSE") {
      // Almost always the luau-lsp extension with its own plugin server on.
      // Both cannot hold the port, and only one of us needs to: whoever has it
      // can forward to the other, which is what `forwardTo` is for. Said once,
      // with the fix in it, rather than thrown.
      options.log(
        `studio plugin: port ${options.port} is already in use, so DataModel info will not ` +
          `reach LuauX. Another extension (usually luau-lsp with "luau-lsp.plugin.enabled") ` +
          `has it. Either move that one to a different port and set "luaux.plugin.forwardTo" ` +
          `to it, or set "luaux.plugin.port" here and point the Studio plugin at that instead.`,
      );
      return;
    }

    options.log(`studio plugin: ${error.message}`);
  });

  // Loopback only. This accepts a DataModel and hands it straight to a type
  // checker; there is no reason for it to be reachable from off the machine.
  server.listen(options.port, "127.0.0.1", () => {
    const downstream = options.forwardTo();
    options.log(
      `studio plugin: listening on 127.0.0.1:${options.port}` +
        (downstream ? `, forwarding to ${downstream}` : ""),
    );
  });

  return { close: () => server.close() };
}

async function handle(
  request: http.IncomingMessage,
  response: http.ServerResponse,
  options: PluginOptions,
  complained: Set<string>,
): Promise<void> {
  const url = (request.url ?? "").split("?")[0];

  if (request.method === "GET" && url === "/get-file-paths") {
    return json(response, 200, { files: await options.listFiles() });
  }

  if (request.method !== "POST") {
    response.writeHead(404);
    return void response.end();
  }

  if (url === "/clear") {
    options.notify("$/plugin/clear");
    forward(url, undefined, options, complained);
    response.writeHead(200);
    return void response.end();
  }

  if (url !== "/full") {
    response.writeHead(404);
    return void response.end();
  }

  let raw: string;
  try {
    raw = await body(request, options.limit);
  } catch (error) {
    // A DataModel bigger than the cap is a real and reported condition, not a
    // crash: luau-lsp's own extension answers 413 and says which setting moves
    // it, because the alternative is a Studio plugin that silently does nothing.
    options.log(`studio plugin: ${message(error)}`);
    response.writeHead(413);
    return void response.end(
      `${message(error)}. Raise "luaux.plugin.maximumRequestBodySize", or reduce the ` +
        `include list in the Studio plugin's settings.`,
    );
  }

  let tree: unknown;
  try {
    tree = (JSON.parse(raw) as { tree?: unknown }).tree;
  } catch {
    response.writeHead(400);
    return void response.end("body was not JSON");
  }

  // luau-lsp answers 400 for this rather than forwarding an empty DataModel,
  // and a plugin that sent a malformed body should hear about it.
  if (tree === undefined) {
    response.writeHead(400);
    return void response.end("no tree in body");
  }

  options.notify("$/plugin/full", tree);
  forward(url, raw, options, complained);

  response.writeHead(200);
  response.end();
}

/// Re-posts what arrived to a second plugin server, so both extensions get the
/// DataModel from a Studio plugin that can only be pointed at one port.
///
/// Failure is logged once per reason and never propagated: the forward is a
/// courtesy to the *other* extension, and letting it fail this request would
/// mean an unreachable neighbour breaks our own DataModel too.
function forward(
  path: string,
  raw: string | undefined,
  options: PluginOptions,
  complained: Set<string>,
): void {
  const port = options.forwardTo();

  if (!port) {
    return;
  }

  const downstream = http.request(
    {
      host: "127.0.0.1",
      port,
      path,
      method: "POST",
      headers: {
        "content-type": "application/json",
        "content-length": raw ? Buffer.byteLength(raw) : 0,
      },
      timeout: 5000,
    },
    (response) => response.resume(),
  );

  const failed = (reason: string) => {
    downstream.destroy();

    // `Set.add` returns the set, which is always truthy — the "say it once"
    // has to be a `has` check or it says it on every DataModel, forever.
    const key = `${port}:${reason}`;
    if (complained.has(key)) {
      return;
    }
    complained.add(key);
    options.log(`studio plugin: could not forward to ${port} (${reason})`);
  };

  downstream.on("timeout", () => failed("timed out"));
  downstream.on("error", (error) => failed(error.message));

  downstream.end(raw ?? "");
}

/// The request body, refusing anything over `limit` **as it arrives**.
///
/// The cap has to be enforced while reading rather than after it: a DataModel
/// from a large place file is tens of megabytes, and buffering the whole thing
/// to discover it was too big is the memory the cap exists to avoid spending.
function body(request: http.IncomingMessage, limit: number): Promise<string> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    let size = 0;

    const onData = (chunk: Buffer) => {
      size += chunk.length;

      if (size <= limit) {
        chunks.push(chunk);
        return;
      }

      // Drop what was buffered — releasing it is the whole point of the cap —
      // but **do not destroy the socket**. The refusal is only useful if it
      // arrives, and a destroyed socket gives the Studio plugin an ECONNRESET
      // instead of the 413 that names the setting to raise. Draining without a
      // `data` listener discards the rest at no cost.
      chunks.length = 0;
      request.off("data", onData);
      request.resume();

      reject(new Error(`DataModel is larger than the ${limit}-byte limit`));
    };

    request.on("data", onData);

    request.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
    request.on("error", reject);
  });
}

function json(response: http.ServerResponse, status: number, value: unknown): void {
  const text = JSON.stringify(value);
  response.writeHead(status, {
    "content-type": "application/json",
    "content-length": Buffer.byteLength(text),
  });
  response.end(text);
}

/// `"3mb"` → bytes, matching the spelling luau-lsp's own setting uses.
///
/// Anything unparseable falls back to the default rather than to zero: a typo
/// in a setting should cost the setting, not every DataModel the plugin sends.
export function bytes(size: string | undefined): number {
  const fallback = 3 * 1024 * 1024;

  if (!size) {
    return fallback;
  }

  const match = /^\s*([\d.]+)\s*(b|kb|mb|gb)?\s*$/i.exec(size);
  if (!match) {
    return fallback;
  }

  const scale = { b: 1, kb: 1024, mb: 1024 ** 2, gb: 1024 ** 3 }[
    (match[2] ?? "b").toLowerCase() as "b" | "kb" | "mb" | "gb"
  ];

  const value = Number(match[1]) * scale;
  return Number.isFinite(value) && value > 0 ? Math.floor(value) : fallback;
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
