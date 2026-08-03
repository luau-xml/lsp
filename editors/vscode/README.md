# LuauX — VS Code

Editor support for `.luaux` — [LuauX](https://github.com/luau-xml/luaux),
JSX-style markup for Luau.

Highlighting works on its own. Completion, diagnostics, hover and the rest come
from `luaux-lsp`, which the extension starts if it can find one.

## What you get

Typing `<Fra` offers the Roblox class list and your own components; `<TextLabel `
offers that class's properties and events; `</` closes what is open. All of it
works while the file does not compile, because a file being typed never does.

Compile errors appear as you type, with one-click fixes for `<Frmae>` and for a
property the class does not have. Hovering a tag says whether it resolved to a
Roblox class or to a component, and where that component is bound — something
the generated `create("Frame")` cannot tell you, because by then it is a string.

Install [luau-lsp](https://marketplace.visualstudio.com/items?itemName=JohnnyMorganz.luau-lsp)
alongside this and everything inside `{ … }` works too: its types, its
completions, its diagnostics, all reported against the `.luaux` you wrote. Your
existing `luau-lsp.*` settings are read and passed through, so definition files
are configured once.

The two extensions do not conflict. This one claims `.luaux` and only `.luaux`.

## Settings

| | |
| --- | --- |
| `luaux.server.path` | Path to `luaux-lsp`. Empty searches `PATH`, then rokit's tool storage, then the bundled copy. |
| `luaux.luauLsp.path` | Path to `luau-lsp`. Empty searches the same places. |
| `luaux.trace.server` | Log traffic between the editor and the server. |

## How the highlighting works

`.luaux` is Luau plus LuauX, so the base grammar includes `source.luau` wholesale
and the LuauX rules arrive as an **injection**.

Injection rather than layering is not a stylistic choice. A layered grammar stops
applying once `source.luau` enters one of its own begin/end blocks — and
`return (<Frame/>)` puts the LuauX inside a parenthesised expression, so the most
common form of all was the broken one.

The grammar stays even though the server emits semantic tokens, because it is the
fallback for whenever the server is starting, crashed or absent. Highlighting
must never depend on a process being alive.

## Known limitation

TextMate is regex-only, so it cannot do the previous-token analysis the compiler
uses to tell `<Frame/>` from `a < b`. The heuristic is that a tag is `<`
immediately followed by a name and then whitespace, `/` or `>`.

`a < b` is safe, because of the space. `a <b` will highlight as a tag. That is
cosmetic — the compiler remains the authority on what is LuauX, and the server
uses the compiler's own rule rather than this one.

## Tests

The grammar is tokenised with the real TextMate engine and asserted on:

```sh
cd test && npm install && npm test
```

Both bugs found while writing it — a parent element closed by a self-closing
child, and nested tags flattened by a missing `\G` anchor — are pinned there.

## Building

```sh
npm install && npm run compile
```

## Installing locally

```sh
ln -s "$PWD" ~/.vscode/extensions/luaux-0.1.0
```

Then reload VS Code.
