<div align="center">

# LuauX LSP

**Editor support for `.luaux`**

Syntax highlighting and a language server for
[LuauX](https://github.com/luau-xml/luaux) — JSX syntax in Luau.

[![CI](https://github.com/luau-xml/lsp/actions/workflows/ci.yml/badge.svg)](https://github.com/luau-xml/lsp/actions/workflows/ci.yml)

</div>

## Overview

Highlighting works on its own. Completion, diagnostics, hover and the rest come
from `luaux-lsp`, which this extension starts if it can find one, and which ships
inside it so that it can.

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
components bound in the file. `<TextLabel ` offers that class's properties and
events. `</` closes whatever is open. All of it works while the file does not
compile, because a file being typed never does.

Hovering `<Card>` says it resolved to a component and where. That is something
`create("Frame")` cannot tell you, because by then it is a string.

A misspelling is reported where it was written, with a one-click fix that inserts
the project's own spelling:

```
<Frmae> is not a Roblox class and is not defined
  → Change to <Frame>
```

## What it does

**On its own**, with no other extension and no second process:

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

**With [luau-lsp](https://marketplace.visualstudio.com/items?itemName=JohnnyMorganz.luau-lsp)
installed**, everything inside a captured `{ … }` as well:

- Hover, completion, definition, references, signature help, inlay hints
- Its type errors, merged with ours and mapped back onto the `.luaux`

## Using it with luau-lsp

The two extensions do not conflict. This one claims `.luaux` and only `.luaux`;
luau-lsp keeps `.luau`. Install both and your existing `luau-lsp.*` settings are
read and passed through, so definition files and FFlags are configured once
rather than twice.

Without luau-lsp, everything above still works except the parts that needed Luau
types. Nothing fails, and the LuauX output channel says what it found.

```
src/App.luaux          ← you edit this, and it is what the editor shows
  │
  ├─ luaux-lsp         ← owns the file, answers markup questions
  │    └─ luau-lsp     ← stock binary, unmodified, answers Luau questions
  │
  └─ luaux build       ← the compiler, unchanged
```

The generated `build/App.luau` is the path the build already writes, so `require`
resolves, the rojo sourcemap lines up, and `.luaurc` aliases apply. As far as
luau-lsp is concerned, this is the file it would have analysed anyway.

## Configuration

Every setting is optional.

| Setting                    | Description                                                                                            |
| -------------------------- | ------------------------------------------------------------------------------------------------------ |
| `luaux.server.path`        | Path to `luaux-lsp`. Empty searches `PATH`, then rokit's tool storage, then the bundled copy.           |
| `luaux.luauLsp.path`       | Path to `luau-lsp`. Empty searches the same places. Without one, LuauX answers nothing that needed Luau types. |
| `luaux.autoClosingTags`    | Close a tag as you finish opening it: typing `<Frame>` puts `</Frame>` after the cursor. Default `true`. |
| `luaux.completion.enabled` | Offer completions. Turn it off to leave them to another language server; every other feature keeps working. Default `true`. |
| `luaux.trace.server`       | Log traffic between the editor and the LuauX server. `off`, `messages`, or `verbose`.                  |

`LuauX: Restart Server` restarts it from the command palette. The LuauX output
channel reports which binaries were chosen, which is the first place to look if
something is missing.

## How the highlighting works

`.luaux` is Luau plus LuauX, so the base grammar includes `source.luau` wholesale
and the LuauX rules arrive as an **injection**.

Injection rather than layering is not a stylistic choice. A layered grammar stops
applying once `source.luau` enters one of its own begin/end blocks, and
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
cosmetic: the compiler remains the authority on what is LuauX, and the server
uses the compiler's own rule rather than this one.

## Contributing

Source, build instructions and the test suites are at
[luau-xml/lsp](https://github.com/luau-xml/lsp). Bugs and requests go to the
[issue tracker](https://github.com/luau-xml/lsp/issues).

## Credits

Built on [luau-lsp](https://github.com/JohnnyMorganz/luau-lsp) by JohnnyMorganz,
which answers every Luau question here, unmodified, unforked, and unaware that
LuauX exists.

This project is not affiliated with or endorsed by luau-lsp.

## License

MIT.
