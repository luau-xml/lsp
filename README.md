# luaux-lsp

Editor support for `.luaux` — syntax highlighting and a language server for
[LuauX](https://github.com/luau-xml/luaux), JSX-style markup for Luau.

```
src/App.luaux          ← you edit this, and it is what the editor shows
  │
  ├─ luaux-lsp         ← this repo: owns the file, answers markup questions
  │    └─ luau-lsp     ← stock binary, unmodified, answers Luau questions
  │
  └─ luaux build       ← the compiler, unchanged
```

`.luaux` is invisible to every tool a Roblox developer already has. The compiler
answers that by compiling out of tree: rojo syncs `build/`, luau-lsp and selene
analyse `build/`, and line-preserving codegen means a diagnostic on
`build/App.luau:42` is genuinely about `src/App.luaux:42`.

That is enough to ship. It is not enough to feel good — a developer typing
`<TextLabel Te` gets nothing, in a file format whose whole premise is that
writing UI should be pleasant. This repository closes that gap.

## What it does

**Without luau-lsp** — no proxy, no source map, no second process:

- Diagnostics from the compiler, with `help` as related information
- Quick fixes: `<Frmae>` → `<Frame>`, and unknown properties → the nearest real ones
- Completion: tag names, a class's properties and events, and closing tags —
  all alias-aware, and all working on files that do not compile
- Hover on a tag: whether it resolved to a Roblox class or a component, and
  where that component is bound
- Go-to-definition on a component tag
- Document symbols: the element tree as an outline
- Semantic tokens telling an intrinsic apart from a component — something a
  regex grammar cannot know
- Rename on a tag pair, `<Frame>…</Frame>` together

**With luau-lsp**, everything inside a captured `{ … }` as well:

- Hover, completion, definition, references, signature help, inlay hints
- Its type errors, merged with ours and mapped back onto the `.luaux`

## How it works

The pattern is settled prior art — Svelte, Vue, Astro and MDX all do it. Own the
file, compile it in memory, forward Luau questions to the real server with
positions translated both ways.

Two things make it cheap here. The compiler **preserves lines**, so line *N* of
the `.luaux` is line *N* of the `.luau`. And expressions are **captured
verbatim**, never parsed, so `Size={UDim2.fromScale(1, 1)}` puts that exact
substring in the output. The map is therefore a short list of runs of identical
text, and every position worth forwarding lies inside one — because the things
that do not map are precisely the things this server answers itself.

Generated text — `create(`, `Text = `, the `__luaux_read` wrapper — has no source
counterpart. Positions there map to nothing, and callers drop them rather than
snapping to the nearest run. A wrong position sends people to code they did not
write.

luau-lsp is handed `build/App.luau`, **the path the build already writes**. So
`require` resolves, the rojo sourcemap lines up, `.luaurc` aliases apply, and
definition files apply — as far as it is concerned this is the file it would
have analysed anyway.

## Layout

```
server/            Rust — the language server binary (luaux-lsp)
editors/vscode/    the extension: grammar, client, packaging
  syntaxes/        luaux.tmLanguage.json + luaux.injection.json
  src/             TypeScript client — locate the binary, start the client
  test/            grammar tokenization tests
```

## Building

The server needs the compiler as a *library* — spans, the resolver's bindings,
the Roblox class tables. Clone it beside this repository:

```sh
git clone https://github.com/luau-xml/luaux ../luaux
cargo build --release
```

`LUAUX_PIN` records the revision this is built and tested against. **Pin, never
float**: a server built against a different compiler than the one producing
`build/` reports diagnostics the build does not, which is worse than reporting
none. The version it was built against is baked in and reported by
`luaux-lsp --version`, and the extension warns when it disagrees with the
`luaux` on your `PATH`.

The extension:

```sh
cd editors/vscode && npm install && npm run compile
```

## Testing

```sh
cargo test                              # unit, plus the protocol tests
cd editors/vscode/test && npm test      # 33 grammar tokenization checks
```

The tests that need a real luau-lsp find one on `PATH` or in rokit's tool
storage, and **skip with a reason** when there is none — integration claims that
are never executed are not claims. Point them somewhere specific with
`LUAU_LSP=/path/to/luau-lsp cargo test`.

Two invariants are worth naming, because everything else leans on them:

- **The map round-trips.** For every run, `to_source(to_output(n)) == n`. Runs
  are checked against both files as they are recorded, so a shape the builder
  does not recognise costs coverage and can never produce a wrong position.
- **The grammar's 33 checks.** Every highlighting fix adds one.

## Installing locally

```sh
ln -s "$PWD/editors/vscode" ~/.vscode/extensions/luaux-0.1.0
```

Then reload VS Code. It looks for `luaux-lsp` on `PATH`, then in rokit's tool
storage, then bundled — and says which it chose in the LuauX output channel.

Without it, highlighting still works. That is deliberate: the grammar is the
zero-latency fallback for whenever the server is starting, crashed or absent,
and highlighting must never depend on a process being alive.

## Coexisting with the luau-lsp extension

They do not conflict. This one claims `.luaux` and only `.luaux`; the luau-lsp
extension keeps `.luau`. Install both — this one reads your existing
`luau-lsp.*` settings and passes them to the child, so definition files and
FFlags are configured once.

## License

MIT.
