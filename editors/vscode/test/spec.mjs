import fs from "fs";
import { tokenize } from "./tokenize.mjs";

let failures = 0;
let checks = 0;

/** Assert the scope stack for `text` contains `scope`. */
function scoped(label, source, text, scope) {
  checks++;
  const tokens = tokenize(source);
  const hit = tokens.find(([t]) => t === text);
  const ok = hit && hit[1].some((s) => s.startsWith(scope));
  if (!ok) {
    failures++;
    console.log(`  FAIL  ${label}`);
    console.log(`        ${JSON.stringify(text)} -> ${hit ? hit[1].join(" ") : "(no such token)"}`);
  }
}

function notScoped(label, source, text, scope) {
  checks++;
  const tokens = tokenize(source);
  const hit = tokens.find(([t]) => t === text);
  const bad = hit && hit[1].some((s) => s.startsWith(scope));
  if (bad) {
    failures++;
    console.log(`  FAIL  ${label}`);
    console.log(`        ${JSON.stringify(text)} unexpectedly ${scope} (${hit[1].join(" ")})`);
  }
}

// --- tags ---
scoped("element name", 'local e = <Frame/>', "Frame", "entity.name.tag");
scoped("member name", 'local e = <Foo.Bar/>', "Foo.Bar", "entity.name.tag");
scoped("closing tag name", 'local e = <Frame></Frame>', "Frame", "entity.name.tag");
scoped("aliased lowercase tag", 'local e = <text/>', "text", "entity.name.tag");

// --- nesting: the bug the \G anchor fixed ---
{
  const source = 'local e = <Frame><TextLabel></TextLabel></Frame>';
  const tokens = tokenize(source);
  const closings = tokens.filter(([t, s]) => t === "</" && s.some((x) => x.startsWith("punctuation.definition.tag")));
  checks++;
  if (closings.length !== 2) {
    failures++;
    console.log(`  FAIL  both closing tags recognised (got ${closings.length})`);
  }
  // The outer element must still be open around the inner one.
  const inner = tokens.find(([t]) => t === "TextLabel");
  checks++;
  if (!inner || inner[1].filter((s) => s === "meta.tag.luaux").length < 2) {
    failures++;
    console.log(`  FAIL  inner element nests inside the outer (${inner ? inner[1].join(" ") : "missing"})`);
  }
}

// --- attributes ---
scoped("attribute name", 'local e = <Frame Name="a"/>', "Name", "entity.other.attribute-name");
scoped("attribute string", 'local e = <Frame Name="a"/>', "a", "string.quoted");
scoped("shorthand attribute", 'local e = <Frame Visible />', "Visible", "entity.other.attribute-name");
scoped("attribute expression is luau", 'local e = <Frame Size={UDim2.new()}/>', "UDim2", "");
notScoped("attribute expression is not text", 'local e = <Frame Size={x}/>', "x", "string.unquoted");

// --- text: the apostrophe case ---
scoped("text content", "local e = <TextLabel>don't</TextLabel>", "don't", "string.unquoted");
notScoped("apostrophe does not open a luau string", "local e = <TextLabel>don't</TextLabel>\nlocal after = 1", "after", "string");

// --- interpolation ---
scoped("expression child is luau", 'local e = <TextLabel>Hi {count}</TextLabel>', "count", "variable");
scoped("text beside an expression", 'local e = <TextLabel>Hi {count}</TextLabel>', "Hi ", "string.unquoted");

// --- fragments, comments, nesting back into luau ---
scoped("fragment open", 'local e = (<><Frame/></>)', "<", "punctuation.definition.tag");
scoped("luaux comment", "local e = <Frame><!-- note --></Frame>", " note ", "comment");

// `<--` collides with valid Luau — `a <--[[c]] b` is `a < b` with a block
// comment — and a regex-only highlighter cannot tell them apart, so it used to
// swallow the rest of the file. `<!--` cannot collide: `!` is not a Luau token.
notScoped("a <--[[c]] b is not a luaux comment", "local ok = a <--[[c]] b\nlocal after = 1", "after", "comment.block.luaux");
scoped("that line stays luau", "local ok = a <--[[c]] b\nlocal after = 1", "after", "variable");
scoped("nested luaux inside an expression", 'local e = <Frame>{c and <TextLabel/> or nil}</Frame>', "TextLabel", "entity.name.tag");
scoped("table literal inside an expression", 'local e = <Frame Size={{1, 2}}/>', "1", "constant.numeric");

// --- the heuristic: comparisons must survive ---
notScoped("a < b is not a tag", "local ok = a < b", "b", "entity.name.tag");
notScoped("a < b keeps luau scope", "local ok = a < b", "<", "punctuation.definition.tag");

// A `<` that follows something an expression can *end* with — a name, a `)`, a
// `]` — is a comparison or a type argument, never a tag. Without this, generics
// open an element that never closes and the rest of the file is swallowed as tag
// text, which is the loudest way a highlighter can fail.
notScoped("a generic parameter is not a tag", "type Source<T> = Vide.Source<T>", "T", "entity.name.tag");
notScoped("a generic on a member type is not a tag", "type S = Vide.Source<T>", "<", "punctuation.definition.tag");
notScoped("a call compared to a value is not a tag", "local ok = f()<g", "g", "entity.name.tag");
notScoped("an index compared to a value is not a tag", "local ok = t[1]<g", "g", "entity.name.tag");

// And nothing above may cost a tag that really is one.
scoped("a tag after `=` still opens", "local e = <Frame/>", "Frame", "entity.name.tag");
scoped("a tag after `(` still opens", "return (<Frame/>)", "Frame", "entity.name.tag");
scoped("a tag after `,` still opens", "f(a, <Frame/>)", "Frame", "entity.name.tag");
scoped("a tag after `>` still opens", "local e = <Frame><TextLabel/></Frame>", "TextLabel", "entity.name.tag");
scoped("a tag after `}` still opens", "local e = <Frame>{x}<TextLabel/></Frame>", "TextLabel", "entity.name.tag");
scoped("a fragment still opens", "local e = (<><Frame/></>)", "Frame", "entity.name.tag");

// The guard is for *entering* markup from Luau. Between children the character
// before a `<` is ordinary text, and guarding there would stop the child — whose
// `/>` is then eaten by the parent's `end`, silently closing it and leaving the
// real closing tag to tokenize as Luau operators.
scoped("a child tag straight after text opens", 'local e = <TextLabel>Hi<Row/></TextLabel>', "Row", "entity.name.tag");
{
  const source = 'local e = <TextLabel>Total<Row Text={n}/></TextLabel>';
  const tokens = tokenize(source);
  checks++;
  const closing = tokens.find(([t, s]) => t === "</" && s.some((x) => x.startsWith("punctuation.definition.tag")));
  if (!closing) {
    failures++;
    console.log("  FAIL  a child after text does not close its parent");
    console.log(`        ${tokens.map(([t]) => JSON.stringify(t)).join(" ")}`);
  }
}

// The file that found this: a generic on line 1, markup much later. Everything
// after the generic has to still be Luau — asserted on a token that exists in
// *both* readings, since a swallowed line merges into one token and an assertion
// naming a token that is simply absent passes for the wrong reason.
{
  const source = [
    "type Source<T> = Vide.Source<T>",
    "local m: Map<string, number> = f()",
    "local e = (<TextLabel>Hi</TextLabel>)",
  ].join("\n");

  scoped("markup after a generic still highlights", source, "TextLabel", "entity.name.tag");
  scoped("a type annotation after a generic is still Luau", source, "Map", "entity.name.type");
  notScoped("a type annotation is not a tag", source, "string", "entity.name.tag");
  // One `meta.tag.luaux` deep, not three: the generics must not have opened
  // elements that never close and nest everything after them.
  {
    checks++;
    const hit = tokenize(source).find(([t]) => t === "TextLabel");
    const depth = hit ? hit[1].filter((s) => s === "meta.tag.luaux").length : -1;
    if (depth !== 1) {
      failures++;
      console.log(`  FAIL  markup after a generic is not nested inside it (depth ${depth})`);
    }
  }
}

// The two entry rules differ by the lookbehind and nothing else. They are
// separate only because a TextMate include cannot take a parameter, and a fix
// applied to one and not the other is exactly how this breaks again.
{
  const grammar = JSON.parse(
    fs.readFileSync(new URL("../syntaxes/luaux.injection.json", import.meta.url), "utf8")
  );
  const GUARD = "(?<![_A-Za-z0-9)\\]])";

  for (const name of ["element", "fragment"]) {
    checks++;
    const shared = { ...grammar.repository[name] };
    const entering = { ...grammar.repository[`${name}EnteringMarkup`] };

    const drifted =
      entering.begin !== GUARD + shared.begin ||
      JSON.stringify({ ...shared, begin: 0, comment: 0 }) !==
        JSON.stringify({ ...entering, begin: 0, comment: 0 });

    if (drifted) {
      failures++;
      console.log(`  FAIL  ${name}EnteringMarkup has drifted from ${name}`);
    }
  }
}
notScoped("less-or-equal is not a tag", "local ok = a <= b", "<=", "punctuation.definition.tag");
notScoped("generic type is not a tag", "local m: Map<string, number> = f()", "string", "entity.name.tag");

// --- regressions found while building this ---

// A layered grammar stopped applying once source.luau entered its own paren
// block, so the most common form of all was the broken one. Hence injection.
scoped("luaux inside parentheses", "local e = (<Frame/>)", "Frame", "entity.name.tag");
scoped("luaux in a return", "return (\n  <Frame/>\n)", "Frame", "entity.name.tag");
scoped("luaux inside a table", "local t = { <Frame/> }", "Frame", "entity.name.tag");
scoped("luaux inside a call", "f(<Frame/>)", "Frame", "entity.name.tag");

// A self-closing *child* used to close its parent, because the parent ended on
// a zero-width `(?<=/>)` that the child's `/>` also satisfied.
{
  const source = "local e = <Frame><UICorner/></Frame>";
  const tokens = tokenize(source);
  const closing = tokens.filter(([t]) => t === "</");
  checks++;
  if (closing.length !== 1) {
    failures++;
    console.log(`  FAIL  self-closing child does not close its parent (got ${closing.length} closing tags)`);
  }
  const frames = tokens.filter(([t, s]) => t === "Frame" && s.some((x) => x.startsWith("entity.name.tag")));
  checks++;
  if (frames.length !== 2) {
    failures++;
    console.log(`  FAIL  parent open and close both tagged (got ${frames.length})`);
  }
}

// Multi-line self-closing: a lookahead cannot see across lines, which is why
// the element consumes `/>` instead of testing for it.
scoped("multi-line self-closing tag", 'local e = (\n  <Button\n    Label="x"\n  />\n)', "Button", "entity.name.tag");
{
  const tokens = tokenize('local e = (\n  <Button\n    Label="x"\n  />\n)\nlocal after = 1');
  const after = tokens.find(([t]) => t === "after");
  checks++;
  if (!after || after[1].some((s) => s.startsWith("meta.tag"))) {
    failures++;
    console.log(`  FAIL  multi-line self-closing tag closes (${after ? after[1].join(" ") : "missing"})`);
  }
}

console.log(`\n${checks} check(s), ${failures} failure(s)`);
process.exit(failures > 0 ? 1 : 0);
