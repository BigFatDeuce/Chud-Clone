// Generates the notice of the third-party components shipped in the binaries (normal
// dependencies of chud-clone-app and chud-clone-cli for Windows x64), with their license texts.
// Usage: node scripts/third-party.js <output.txt>
"use strict";
const { execFileSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const out = process.argv[2];
if (!out) { console.error("usage: node scripts/third-party.js <output.txt>"); process.exit(2); }
const meta = JSON.parse(execFileSync("cargo", ["metadata", "--format-version", "1", "--filter-platform", "x86_64-pc-windows-msvc"], { maxBuffer: 1 << 28, cwd: path.join(__dirname, "..") }).toString());
const pkgs = new Map(meta.packages.map((p) => [p.id, p]));
const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]));
const roots = meta.packages.filter((p) => ["chud-clone-app", "chud-clone-cli"].includes(p.name)).map((p) => p.id);
const seen = new Set();
const stack = [...roots];
while (stack.length) {
  const id = stack.pop();
  if (seen.has(id)) continue;
  seen.add(id);
  for (const d of nodes.get(id)?.deps || []) {
    // Normal dependencies only: no build tools, no tests.
    if (d.dep_kinds.some((k) => k.kind === null)) stack.push(d.pkg);
  }
}
const ours = new Set(meta.workspace_members);
const list = [...seen].filter((id) => !ours.has(id)).map((id) => pkgs.get(id)).sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));
const texts = new Map(); // deduplicates identical license texts
let body = "";
for (const p of list) {
  const dir = path.dirname(p.manifest_path);
  const files = fs.readdirSync(dir).filter((f) => /^(licen[cs]e|copying|notice|copyright)/i.test(f)).sort();
  body += `\n${"=".repeat(78)}\n${p.name} ${p.version}\nLicense: ${p.license || p.license_file || "not stated"}\n${p.repository ? "Source: " + p.repository + "\n" : ""}`;
  for (const f of files) {
    const full = path.join(dir, f);
    if (!fs.statSync(full).isFile()) continue;
    const t = fs.readFileSync(full, "utf8").trim();
    const key = t.replace(/\s+/g, " ");
    if (texts.has(key)) body += `\n[${f}: same text as ${texts.get(key)}]\n`;
    else { texts.set(key, `${p.name} ${p.version}`); body += `\n--- ${f}\n${t}\n`; }
  }
  if (!files.length) body += `\n(No license file in the package; declared license: ${p.license || "not stated"}.)\n`;
}
const head = `Chud Clone — third-party components\n\nchud-clone.exe and chud-clone-cli.exe include the following ${list.length} open-source\ncomponents, used under the terms of their licenses reproduced below.\n`;
fs.writeFileSync(out, head + body);
console.log(`${list.length} components → ${out}`);
