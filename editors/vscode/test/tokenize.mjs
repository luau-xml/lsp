import fs from "fs";
import os from "os";
import path from "path";
import url from "url";
import onigModule from "vscode-oniguruma";
const oniguruma = onigModule.default ?? onigModule;
import tmModule from "vscode-textmate";
const textmate = tmModule.default ?? tmModule;

// `fileURLToPath`, not `.pathname`: on Windows the latter is
// `/C:/Users/Ryan%20Cundiff/…`, which joins into `C:\C:\Users\Ryan%20Cundiff\…`
// and opens nothing.
const HERE = path.dirname(url.fileURLToPath(import.meta.url));
const EXT = path.resolve(HERE, "..");
// The Luau grammar comes from the luau-lsp extension. Version-agnostic so this
// keeps working across upgrades.
function findLuauGrammar() {
  const override = process.env.LUAUX_LUAU_GRAMMAR;
  if (override) return override;

  // A CI runner has no VS Code at all, so this is an ordinary absence rather
  // than an error worth a stack trace. `LUAUX_LUAU_GRAMMAR` is how CI supplies
  // it; the message below says so.
  // `homedir()` rather than `$HOME`, which Windows does not set — the same
  // reason the server reaches for `home_dir` there.
  const root = path.join(os.homedir(), ".vscode/extensions");
  let installed = [];
  try {
    installed = fs.readdirSync(root);
  } catch {
    installed = [];
  }

  const match = installed
    .filter((name) => name.startsWith("johnnymorganz.luau-lsp-"))
    .sort()
    .pop();

  if (!match) {
    throw new Error(
      "luau-lsp extension not found; set LUAUX_LUAU_GRAMMAR to a Luau.tmLanguage.json"
    );
  }
  return path.join(root, match, "syntaxes/Luau.tmLanguage.json");
}
const LUAU_GRAMMAR = findLuauGrammar();

const wasm = fs.readFileSync(
  path.join(HERE, "node_modules/vscode-oniguruma/release/onig.wasm")
);
await oniguruma.loadWASM(wasm.buffer);

const registry = new textmate.Registry({
  onigLib: Promise.resolve({
    createOnigScanner: (s) => new oniguruma.OnigScanner(s),
    createOnigString: (s) => new oniguruma.OnigString(s),
  }),
  // Injections apply inside Luau's own blocks, which a layered grammar cannot.
  getInjections: (scope) => (scope === "source.luaux" ? ["luaux.injection"] : []),
  loadGrammar: async (scope) => {
    const file =
      scope === "source.luaux"
        ? path.join(EXT, "syntaxes/luaux.tmLanguage.json")
        : scope === "luaux.injection"
        ? path.join(EXT, "syntaxes/luaux.injection.json")
        : scope === "source.luau"
        ? LUAU_GRAMMAR
        : null;
    if (!file) return null;
    return textmate.parseRawGrammar(fs.readFileSync(file, "utf8"), file);
  },
});

const grammar = await registry.loadGrammar("source.luaux");
if (!grammar) throw new Error("failed to load source.luaux");

/** Tokenize and return [text, scopes[]] pairs, dropping whitespace-only tokens. */
export function tokenize(source) {
  const out = [];
  let state = textmate.INITIAL;
  for (const line of source.split("\n")) {
    const result = grammar.tokenizeLine(line, state);
    for (const token of result.tokens) {
      const text = line.substring(token.startIndex, token.endIndex);
      if (text.trim() === "") continue;
      out.push([text, token.scopes]);
    }
    state = result.ruleStack;
  }
  return out;
}

