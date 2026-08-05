// Packages a `.vsix` for one platform, and says which.
//
// The extension bundles a server binary, and a binary is for exactly one
// platform. An untagged `.vsix` therefore installs anywhere and works in one
// place: on any other, VS Code starts a Mach-O on Linux, gets
// `cannot execute binary file`, and restarts five times before giving up —
// reporting `write EPIPE`, which names neither the binary nor the reason.
//
// `--target` puts the platform in the manifest, so VS Code refuses to install a
// mismatched build rather than failing at run time. The filename carries it too,
// so the wrong one is harder to hand over by accident.
//
// The bundled binary is checked before packaging: when building for the host —
// the ordinary case — one that will not start here will not start there either,
// and finding that out now is free.
//
// A version carrying a prerelease suffix is packaged with `--pre-release`, which
// records it in the manifest. It has to happen *here*: `vsce publish` refuses the
// flag on a `.vsix` that was not built with it, and `ovsx publish` ignores the
// flag outright for a prepackaged one. Unmarked, a canary reaches OpenVSX looking
// like an ordinary release and is offered to everyone who never asked for it.

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import url from "node:url";

const TARGETS = {
  "darwin-arm64": "darwin-arm64",
  "darwin-x64": "darwin-x64",
  "linux-arm64": "linux-arm64",
  "linux-x64": "linux-x64",
  "win32-arm64": "win32-arm64",
  "win32-x64": "win32-x64",
};

const host = `${process.platform}-${process.arch}`;
const target = process.argv[2] ?? TARGETS[host];

if (!target) {
  console.error(`no VS Code target for ${host}; pass one explicitly, e.g. linux-x64`);
  process.exit(1);
}

const here = path.dirname(url.fileURLToPath(import.meta.url));
const executable = target.startsWith("win32") ? "luaux-lsp.exe" : "luaux-lsp";
const bundled = path.join(here, "..", "server", executable);

if (!fs.existsSync(bundled)) {
  console.error(
    `no server binary at ${bundled}\n` +
      `Build one for ${target} and copy it there — the extension bundles it.`,
  );
  process.exit(1);
}

// Only meaningful when packaging for this machine; a cross-built binary is not
// expected to run here.
if (target === TARGETS[host]) {
  try {
    execFileSync(bundled, ["--version"], { stdio: "ignore" });
  } catch {
    console.error(
      `the binary at ${bundled} does not run on ${host}.\n` +
        `Packaging it for ${target} would ship something that cannot start.`,
    );
    process.exit(1);
  }
}

// The local one, not whatever a machine happens to have installed: a runner
// has none, and a global install is a different version from the lockfile's.
//
// Its own entry point rather than the `.bin` shim: that shim is `vsce.cmd` on
// Windows, and since the fix for CVE-2024-27980 Node refuses to `execFile` a
// `.cmd` without a shell — `EINVAL`, naming neither the file nor the reason.
// Running the script under this same Node sidesteps the shim on every platform.
const vsce = path.join(here, "..", "node_modules", "@vscode", "vsce", "vsce");

if (!fs.existsSync(vsce)) {
  console.error(`no vsce at ${vsce} — run \`npm install\` first`);
  process.exit(1);
}

// A `-suffix` is the only thing that distinguishes a canary from a release: the
// same rule the workflow uses to decide the Marketplace cannot have it.
const { version } = JSON.parse(fs.readFileSync(path.join(here, "..", "package.json"), "utf8"));
const preRelease = version.includes("-");

console.log(`packaging ${version} for ${target}${preRelease ? " as a pre-release" : ""}`);

const flags = preRelease ? ["--pre-release"] : [];

execFileSync(process.execPath, [vsce, "package", "--target", target, ...flags], {
  stdio: "inherit",
});
