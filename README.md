<div align="center">

# LuauX LSP

**Editor support for `.luaux`**

Syntax highlighting and a language server for
[LuauX](https://github.com/luau-xml/luaux) — JSX syntax in Luau.

[![CI](https://github.com/luau-xml/lsp/actions/workflows/ci.yml/badge.svg)](https://github.com/luau-xml/lsp/actions/workflows/ci.yml)

</div>

## Overview

`.luaux` is invisible to every tool a Roblox developer already has. The compiler
answers that by compiling out of tree: rojo syncs `build/`, luau-lsp and selene
analyse `build/`, and line-preserving codegen means a diagnostic on
`build/App.luau:42` is genuinely about `src/App.luaux:42`.

That is enough to ship. It is not enough to feel good: a developer typing
`<TextLabel Te` gets nothing, in a file format whose whole premise is that
writing UI should be pleasant. This repository closes that gap.

```
src/App.luaux          ← you edit this, and it is what the editor shows
  │
  ├─ luaux-lsp         ← this repo: owns the file, answers markup questions
  │    └─ luau-lsp     ← stock binary, unmodified, answers Luau questions
  │
  └─ luaux build       ← the compiler, unchanged
```

Given a file being written:

```luau
local function Card (props)
  return (
    <Frame Size={UDim2.fromScale(1, 1)}>
      <TextLabel Text={props.Title} />
    </Frame>
  )
end

local card = <Card Title="Hello" />
```

Typing `<Fra` offers `Frame` and every other creatable class, alongside the
components bound in the file. Hovering `<Card>` says it resolved to a component
and where. That is something `create("Frame")` cannot tell you, because by then
it is a string. And inside `{ … }`, `props.Title` has a type, because a real
luau-lsp is answering.

A misspelling is reported where it was written, with a fix that inserts the
project's own spelling:

```
<Frmae> is not a Roblox class and is not defined
  → Change to <Frame>
```

## What it does

**Without luau-lsp** (no proxy, no source map, no second process):

- Diagnostics from the compiler, with `help` as related information
- Quick fixes: `<Frmae>` → `<Frame>`, and unknown properties → the nearest real ones
- Completion: tag names, a class's properties and events, and closing tags,
  all alias-aware, and all working on files that do not compile
- Hover on a tag: whether it resolved to a Roblox class or a component, and
  where that component is bound
- Go-to-definition on a component tag
- Document symbols: the element tree as an outline
- Semantic tokens telling an intrinsic apart from a component, which a regex
  grammar cannot know
- Rename on a tag pair, `<Frame>…</Frame>` together

**With luau-lsp**, everything inside a captured `{ … }` as well:

- Hover, completion, definition, references, signature help, inlay hints
- Its type errors, merged with ours and mapped back onto the `.luaux`

Highlighting works with no server at all. That is deliberate: the grammar is the
zero-latency fallback for whenever the server is starting, crashed or absent, and
highlighting must never depend on a process being alive.

## Installation

> [!NOTE]
> No release is published yet, so building from source is currently the only
> route. The rest of this section describes where the release workflow publishes
> to once a `v*` tag is pushed.

1. From the **[Marketplace](https://marketplace.visualstudio.com/)**. Search for
   `LuauX LSP`, or:

```console
$ code --install-extension RyanCundiff.luaux-lsp
```

2. From **[OpenVSX](https://open-vsx.org/)**, which is what VSCodium and Cursor
   can reach:

```console
$ codium --install-extension RyanCundiff.luaux-lsp
```

3. From a **`.vsix`** on the [releases page](https://github.com/luau-xml/lsp/releases).
   One per platform, because the extension bundles a server binary and a binary
   is for exactly one platform. Take the one matching yours:

```console
$ code --install-extension luaux-lsp-win32-x64-0.1.1.vsix
```

4. **From source**, which needs the compiler as a sibling checkout (see
   [Building](#building)):

```sh
git clone https://github.com/luau-xml/lsp luaux-lsp
git clone https://github.com/luau-xml/luaux          # a sibling, not a submodule
cd luaux-lsp

cargo build --release
mkdir -p editors/vscode/server
cp target/release/luaux-lsp editors/vscode/server/   # luaux-lsp.exe on Windows

cd editors/vscode
npm install && npm run compile && npm run package
code --install-extension luaux-lsp-*.vsix
```

The copy is not incidental: `npm run package` bundles whatever is in
`editors/vscode/server/` and refuses to build a `.vsix` without it.

The extension looks for `luaux-lsp` on `PATH`, then in rokit's tool storage, then
the copy bundled with it, and says which it chose in the LuauX output channel.

## Coexisting with the luau-lsp extension

They do not conflict. This one claims `.luaux` and only `.luaux`; the
[luau-lsp extension](https://marketplace.visualstudio.com/items?itemName=JohnnyMorganz.luau-lsp)
keeps `.luau`. Install both: this one reads your existing `luau-lsp.*` settings
and passes them to the child, so definition files and FFlags are configured once.

## Configuration

Every setting is optional.

| Setting                    | Description                                                                                            |
| -------------------------- | ------------------------------------------------------------------------------------------------------ |
| `luaux.server.path`        | Path to `luaux-lsp`. Empty searches `PATH`, then rokit's tool storage, then the bundled copy.           |
| `luaux.luauLsp.path`       | Path to `luau-lsp`. Empty searches the same places. Without one, LuauX answers nothing that needed Luau types. |
| `luaux.autoClosingTags`    | Close a tag as you finish opening it: typing `<Frame>` puts `</Frame>` after the cursor. Default `true`. |
| `luaux.completion.enabled` | Offer completions. Turn it off to leave them to another language server; every other feature keeps working. Default `true`. |
| `luaux.hover.enabled`      | Answer hovers. Turn it off to leave them to another language server; every other feature keeps working. Default `true`. |
| `luaux.trace.server`       | Log traffic between the editor and the LuauX server. `off`, `messages`, or `verbose`.                  |

`LuauX: Restart Server` restarts it from the command palette.

## How it works

The pattern is settled prior art, and Svelte, Vue, Astro and MDX all do it. Own
the file, compile it in memory, forward Luau questions to the real server with
positions translated both ways.

Two things make it cheap here. The compiler **preserves lines**, so line *N* of
the `.luaux` is line *N* of the `.luau`. And expressions are **captured
verbatim**, never parsed, so `Size={UDim2.fromScale(1, 1)}` puts that exact
substring in the output. The map is therefore a short list of runs of identical
text, and every position worth forwarding lies inside one, because the things
that do not map are precisely the things this server answers itself.

Generated text (`create(`, `Text = `, the `__luaux_read` wrapper) has no source
counterpart. Positions there map to nothing, and callers drop them rather than
snapping to the nearest run. A wrong position sends people to code they did not
write.

luau-lsp is handed `build/App.luau`, **the path the build already writes**. So
`require` resolves, the rojo sourcemap lines up, `.luaurc` aliases apply, and
definition files apply. As far as it is concerned, this is the file it would
have analysed anyway.

## Layout

```
server/            Rust: the language server binary (luaux-lsp)
editors/vscode/    the extension: grammar, client, packaging
  syntaxes/        luaux.tmLanguage.json + luaux.injection.json
  src/             TypeScript client: locate the binary, start the client
  test/            grammar tokenization tests
```

## Building

The server needs the compiler as a *library*: spans, the resolver's bindings,
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
cargo test                              # 267 tests
cd editors/vscode/test && npm test      # 51 grammar tokenization checks
```

The tests that need a real luau-lsp find one on `PATH` or in rokit's tool
storage, and **skip with a reason** when there is none. Integration claims that
are never executed are not claims. Point them somewhere specific with
`LUAU_LSP=/path/to/luau-lsp cargo test`.

Two invariants are worth naming, because everything else leans on them:

- **The map round-trips.** For every run, `to_source(to_output(n)) == n`. Runs
  are checked against both files as they are recorded, so a shape the builder
  does not recognise costs coverage and can never produce a wrong position.
- **The grammar's checks.** Every highlighting fix adds one.

## Status

Early, but real. 267 tests run across Linux, macOS and Windows, of which 23 drive
the proxy against a genuine luau-lsp, because a proxy nothing ever proxies
through is not a tested proxy. The grammar is asserted against the real TextMate
engine, so its checks fail on a regression rather than on a reviewer noticing.

Expect the protocol surface to be stable and the tooling to keep moving.

## Credits

Built on [luau-lsp](https://github.com/JohnnyMorganz/luau-lsp) by JohnnyMorganz,
which answers every Luau question here, unmodified, unforked, and unaware that
LuauX exists.

This project is not affiliated with or endorsed by luau-lsp.

## License

MIT.
