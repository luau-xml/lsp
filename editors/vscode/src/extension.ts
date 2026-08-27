// The client is deliberately thin: find the server, start it, and get out of
// the way. Everything interesting happens in `luaux-lsp`, which is Rust because
// the compiler is a Rust library and reaching it from TypeScript would mean
// WASM or a subprocess — both worse than the framework is good (decision 2).

import * as fs from "node:fs";
import * as https from "node:https";
import * as os from "node:os";
import * as path from "node:path";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";

const run = promisify(execFile);

let client: LanguageClient | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  const configured = vscode.workspace.getConfiguration("luaux").get<string>("server.path");
  const found = await locate("luaux-lsp", configured, context);

  // The grammar still works whatever happens here — highlighting must never
  // depend on a process being alive (decision 5) — so these are notes, not
  // failures. They do have to *say* which thing went wrong, though: without
  // that, a server that cannot start is five crashes and a `write EPIPE`.
  if (found.kind !== "found") {
    vscode.window.showWarningMessage(explain(found));
    return;
  }

  const server = found.command;

  const trace = vscode.workspace.getConfiguration("luaux").get<string>("trace.server") ?? "off";

  const serverOptions: ServerOptions = {
    run: { command: server, args: ["--stdio"], transport: TransportKind.stdio },
    debug: { command: server, args: ["--stdio"], transport: TransportKind.stdio },
  };

  const output = vscode.window.createOutputChannel("LuauX");

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "luaux" }],
    // `.luaux` only. The luau-lsp extension keeps `.luau`, and the two coexist
    // because neither claims the other's files.
    //
    // Both patterns matter to the server and for different reasons.
    // `luaux.toml` decides what compiles at all. Every `.luaux` matters because
    // the server compiles the ones nobody opened — that is what gives a require
    // of one its types — and a file created, deleted or rewritten outside the
    // editor (a branch switch, most often) changes what that require resolves
    // to with no `didChange` to announce it.
    synchronize: {
      fileEvents: [
        vscode.workspace.createFileSystemWatcher("**/luaux.toml"),
        vscode.workspace.createFileSystemWatcher("**/*.luaux"),
      ],
    },
    outputChannel: output,
    traceOutputChannel:
      trace === "off" ? undefined : vscode.window.createOutputChannel("LuauX Trace"),
    // Handed on to luau-lsp by the server, which layers the user's own
    // `luau-lsp.fflags.*` settings over them.
    initializationOptions: { fflags: await syncedFFlags(output) },
  };

  client = new LanguageClient("luaux", "LuauX", serverOptions, clientOptions);
  await client.start();
  context.subscriptions.push(client);

  await reportVersions(server, client);

  context.subscriptions.push(
    vscode.commands.registerCommand("luaux.restartServer", async () => {
      await client?.restart();
      vscode.window.showInformationMessage("LuauX server restarted.");
    }),
    autoCloseTags(client),
  );
}

/// Closes a tag as you finish opening it: type `<Frame>` and `</Frame>` appears
/// after the cursor.
///
/// Done here rather than through on-type formatting because that is off by
/// default in VS Code, and a feature nobody has switched on is not a feature.
/// The *decision* is still the server's — whether a `<` opened a tag at all is
/// the compiler's rule, not something to re-guess with a regex here.
function autoCloseTags(client: LanguageClient): vscode.Disposable {
  return vscode.workspace.onDidChangeTextDocument(async (event) => {
    if (event.document.languageId !== "luaux" || event.contentChanges.length !== 1) {
      return;
    }

    if (!vscode.workspace.getConfiguration("luaux").get<boolean>("autoClosingTags", true)) {
      return;
    }

    // Only the moment a `>` is typed. A paste that happens to end in one is
    // someone moving text around, not opening a tag.
    const change = event.contentChanges[0];
    if (change.text !== ">") {
      return;
    }

    const editor = vscode.window.activeTextEditor;
    if (editor?.document !== event.document) {
      return;
    }

    const position = change.range.start.translate(0, change.text.length);
    const version = event.document.version;

    const answer = await client.sendRequest<{ tagName: string } | null>("luaux/closingTag", {
      textDocument: { uri: event.document.uri.toString() },
      position,
    });

    // Typing does not wait for us, and inserting against a document that has
    // moved on would put the tag somewhere else entirely.
    if (!answer || editor.document.version !== version) {
      return;
    }

    // `$0` leaves the cursor between the tags, which is where the next thing
    // gets typed.
    await editor.insertSnippet(
      new vscode.SnippetString(`$0</${answer.tagName}>`),
      position,
      { undoStopBefore: false, undoStopAfter: true },
    );
  });
}

export async function deactivate(): Promise<void> {
  await client?.stop();
}

const FLAG_SOURCE =
  "https://clientsettingscdn.roblox.com/v1/settings/application?applicationName=PCStudioApp";

/// The type prefixes Roblox's own tables use. luau-lsp names the same flags
/// without them, so `FFlagLuauSolverV2` is `LuauSolverV2` to it.
const FLAG_PREFIXES = ["FFlag", "FInt", "DFFlag", "DFInt"];

/// Roblox's current Luau flag values, as `luau-lsp.fflags.sync` asks for them.
///
/// luau-lsp does not fetch these itself — its own extension does, and hands them
/// to the server through `initializationOptions`. We proxy that server, so the
/// same values have to reach our child or `.luaux` gets *worse* answers than the
/// `.luau` it compiles to, which is exactly backwards. Vide's `create` is the
/// case people hit: it needs the new solver, and without it the whole signature
/// is `*error-type*`.
///
/// Doing it here rather than in the server is deliberate — a network call has no
/// business on the keystroke path, and this is the side that already talks to
/// the editor's configuration.
///
/// Failure is not fatal — flags are an improvement, not a requirement — but it
/// is *said*, because the symptom otherwise is Vide's `create` typed as
/// `*error-type*` with nothing anywhere explaining why. luau-lsp's own extension
/// says the same thing for the same condition.
async function syncedFFlags(output: vscode.OutputChannel): Promise<Record<string, string>> {
  if (!vscode.workspace.getConfiguration("luau-lsp.fflags").get<boolean>("sync", true)) {
    return {};
  }

  let published: Record<string, unknown>;

  try {
    const body = await fetchText(FLAG_SOURCE, 3000);
    published =
      (JSON.parse(body) as { applicationSettings?: Record<string, unknown> }).applicationSettings ??
      {};
  } catch (error) {
    // Activation continues either way: this is the difference between good
    // answers and better ones, not between working and not.
    output.appendLine(
      `could not fetch Luau FFlags (${error instanceof Error ? error.message : error}); ` +
        "continuing without them, so types needing a newer Luau solver may not resolve",
    );
    return {};
  }

  const flags: Record<string, string> = {};

  for (const [name, value] of Object.entries(published)) {
    // Luau's own, and only those: the table holds thousands of flags that mean
    // nothing to a language server.
    const prefix = FLAG_PREFIXES.find((prefix) => name.startsWith(`${prefix}Luau`));
    if (prefix) {
      flags[name.slice(prefix.length)] = String(value);
    }
  }

  return flags;
}

/// `node:https` rather than `fetch`, which is not there on the oldest VS Code
/// this extension claims to support.
///
/// `deadline` is a **total** budget, not an idle timeout. Activation waits on
/// this, and VS Code puts no limit of its own on how long that may take, so a
/// promise that never settles is an extension that never starts — no server, no
/// diagnostics, no completion, and no error to explain it. Node's own
/// `timeout` option is per-socket-idle and is disarmed once the peer destroys
/// the socket, so a response truncated mid-body settles nothing and a slow
/// trickle re-arms it forever. Both are ordinary behaviour for a proxy sitting
/// in front of somebody else's CDN.
function fetchText(url: string, deadline: number): Promise<string> {
  return new Promise((resolve, reject) => {
    let settled = false;

    const finish = (error: Error | undefined, body?: string) => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timer);
      request.destroy();
      error ? reject(error) : resolve(body ?? "");
    };

    const timer = setTimeout(() => finish(new Error(`${url} took longer than ${deadline}ms`)), deadline);

    const request = https.get(url, { timeout: deadline }, (response) => {
      // Redirects are not followed: this URL is a fixed endpoint, and quietly
      // chasing a 3xx somewhere else is not what was asked for.
      if (response.statusCode !== 200) {
        response.resume();
        finish(new Error(`${url} answered ${response.statusCode}`));
        return;
      }

      let body = "";
      response.setEncoding("utf8");
      response.on("data", (chunk) => (body += chunk));
      response.on("end", () => finish(undefined, body));
      // A response that stops early ends the *response*, not the request, and
      // without these the promise is simply abandoned.
      response.on("error", finish);
      response.on("aborted", () => finish(new Error(`${url} ended early`)));
    });

    request.on("timeout", () => finish(new Error(`${url} went idle`)));
    request.on("error", finish);
  });
}

/// What the search turned up, in enough detail to explain it.
type Found =
  | { kind: "found"; command: string }
  /// Present and not runnable. Almost always a VSIX built for another platform.
  | { kind: "unrunnable"; command: string; bundled: boolean }
  | { kind: "missing" };

/// Finds a binary that **starts**: an explicit setting, then `PATH`, then
/// rokit's tool storage, then the one bundled with this extension.
///
/// A project that pins its own tools should get the one it pinned, which is why
/// the bundled copy is last.
///
/// Every candidate is checked by running it, for the reason the server's own
/// discovery gives: "the file is there and executable" is not the same question
/// as "this will start". A `.vsix` carries exactly one platform's binary, so on
/// any other platform that file exists, is executable, and dies with
/// `cannot execute binary file` — and handing it to the LanguageClient anyway
/// buys five restarts and a `write EPIPE` that names neither the binary nor the
/// reason.
async function locate(
  name: string,
  configured: string | undefined,
  context: vscode.ExtensionContext,
): Promise<Found> {
  if (configured && configured.trim().length > 0) {
    // Named explicitly: use it or complain about it. Falling back to a
    // different binary is the version mismatch §2.1 warns about.
    if (!fs.existsSync(configured)) {
      return { kind: "missing" };
    }
    return (await runs(configured))
      ? { kind: "found", command: configured }
      : { kind: "unrunnable", command: configured, bundled: false };
  }

  const executable = process.platform === "win32" ? `${name}.exe` : name;
  const candidates: string[] = [];

  for (const directory of (process.env.PATH ?? "").split(path.delimiter)) {
    const candidate = path.join(directory, executable);
    if (directory && fs.existsSync(candidate)) {
      candidates.push(candidate);
    }
  }

  const stored = rokit(name, executable);
  if (stored) {
    candidates.push(stored);
  }

  const bundled = context.asAbsolutePath(path.join("server", executable));
  const hasBundled = fs.existsSync(bundled);
  if (hasBundled) {
    candidates.push(bundled);
  }

  for (const candidate of candidates) {
    if (await runs(candidate)) {
      return { kind: "found", command: candidate };
    }
  }

  // Something was there and none of it started. Saying so beats "not found",
  // which sends someone off to install what they already have.
  if (candidates.length > 0) {
    return { kind: "unrunnable", command: candidates[candidates.length - 1], bundled: hasBundled };
  }

  return { kind: "missing" };
}

/// Whether a candidate starts and reports a version.
async function runs(candidate: string): Promise<boolean> {
  try {
    await run(candidate, ["--version"], { timeout: 5000 });
    return true;
  } catch {
    return false;
  }
}

/// What to tell someone, given what the search found.
function explain(found: Exclude<Found, { kind: "found" }>): string {
  // Deliberately not "rokit add": there are no published releases yet, and
  // advice that does not work is worse than none. Revisit once releases have
  // actually shipped.
  const install =
    "Build it from the luaux-lsp repository (`cargo build --release`) and either put it on PATH " +
    "or point `luaux.server.path` at it.";

  if (found.kind === "missing") {
    return `luaux-lsp was not found. Syntax highlighting still works; completion and diagnostics need the server. ${install}`;
  }

  if (found.bundled) {
    return (
      `The luaux-lsp bundled with this extension will not run on ${process.platform}/${process.arch} — ` +
      `a .vsix carries one platform's binary, and this one was built for another. ${install}`
    );
  }

  return `luaux-lsp at ${found.command} exists but would not start. ${install}`;
}

/// rokit's shim in `~/.rokit/bin` refuses to run outside a project that lists
/// the tool, so the stored binary is what actually works.
function rokit(name: string, executable: string): string | undefined {
  const storage = path.join(os.homedir(), ".rokit", "tool-storage");
  if (!fs.existsSync(storage)) {
    return undefined;
  }

  let best: { version: string; path: string } | undefined;

  for (const author of readDirectory(storage)) {
    for (const version of readDirectory(path.join(storage, author, name))) {
      const candidate = path.join(storage, author, name, version, executable);
      if (fs.existsSync(candidate) && (!best || version > best.version)) {
        best = { version, path: candidate };
      }
    }
  }

  return best?.path;
}

function readDirectory(at: string): string[] {
  try {
    return fs.readdirSync(at);
  } catch {
    return [];
  }
}

/// Reports which server was chosen, and warns when it and the compiler on
/// `PATH` were built from different versions.
///
/// A server built against a different compiler than the one producing `build/`
/// reports diagnostics the build does not, which is worse than reporting none —
/// and invisible unless someone says it out loud.
async function reportVersions(server: string, client: LanguageClient): Promise<void> {
  const reported = client.initializeResult?.serverInfo?.version;
  client.outputChannel.appendLine(`server: ${server}${reported ? ` (${reported})` : ""}`);

  // `luaux-lsp 0.1.0 (luaux 0.1.0)`
  const built = reported?.match(/luaux ([\w.+-]+)\)/)?.[1];
  if (!built) {
    return;
  }

  const compiler = await compilerVersion();
  if (!compiler) {
    return;
  }

  client.outputChannel.appendLine(`compiler: luaux ${compiler}`);

  if (compiler !== built) {
    vscode.window.showWarningMessage(
      `luaux-lsp was built against luaux ${built}, but luaux ${compiler} is what builds this project. ` +
        "Diagnostics here may not match what `luaux build` reports.",
    );
  }
}

async function compilerVersion(): Promise<string | undefined> {
  try {
    const { stdout } = await run("luaux", ["--version"], { timeout: 5000 });
    return stdout.trim().split(/\s+/).pop();
  } catch {
    return undefined;
  }
}
