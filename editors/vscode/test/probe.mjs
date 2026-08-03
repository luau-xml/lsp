import { tokenize } from "./tokenize.mjs";
// `a <--[[c]] b` is valid Luau: `a < b` with a block comment between.
const src = "local ok = a <--[[c]] b\nlocal after = 1\nlocal more = 2";
for (const [t, s] of tokenize(src)) console.log(JSON.stringify(t).padEnd(16), s.slice(1).join(" "));
