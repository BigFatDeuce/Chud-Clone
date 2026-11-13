// Génère crates/app/ui-dev/mock-data.js à partir de sorties RÉELLES du moteur
// (plan et rapport d'un clonage image → image), pour que le banc d'essai de
// l'interface utilise exactement les structures produites par le moteur.
//
// Usage : node scripts/make-ui-mock-data.js <prep.json> <report.json>
//   prep.json   : chud-clone-cli plan image:<fixture gpt-win.img> image:<dst.img> --image-size 256M --json
//   report.json : chud-clone-cli clone ... --confirm-erase --only-virtual --json
// Les chemins locaux sont remplacés par des chemins fictifs.
"use strict";
const fs = require("fs");
const path = require("path");

const read = (p) => JSON.parse(fs.readFileSync(p, "utf8").replace(/^\uFEFF/, ""));
// Troisième argument « en » : sorties produites avec --lang en, écrites dans mock-data-en.js.
const [prepPath, reportPath, lang] = process.argv.slice(2);
const en = lang === "en";
if (!prepPath || !reportPath) {
  console.error("usage : node scripts/make-ui-mock-data.js <prep.json> <report.json>");
  process.exit(2);
}
const sanitize = (text) =>
  text
    .replace(/\\\\\?\\[a-z]:\\\\[^"]*?gpt-win\.img/gi, "\\\\?\\\\d:\\\\images\\\\source.img")
    .replace(/[A-Z]:\\\\[^"]*?gpt-win\.img/gi, "D:\\\\Images\\\\source.img")
    .replace(/[A-Za-z]:\\\\[^"]*?dst\.img/g, "D:\\\\Images\\\\clone.img")
    .replace(/\\\\\?\\[a-z]:\\\\[^"]*?dst\.img/gi, "\\\\?\\\\d:\\\\images\\\\clone.img")
    .replace(/[A-Za-z]:\\\\Users\\\\[^\\"]+/g, en ? "C:\\\\Users\\\\user" : "C:\\\\Users\\\\utilisateur");
const data = { prep: read(prepPath), report: read(reportPath) };
const out = path.join(__dirname, "..", "crates", "app", "ui-dev", en ? "mock-data-en.js" : "mock-data.js");
fs.writeFileSync(out, "// Généré par scripts/make-ui-mock-data.js — ne pas modifier à la main.\nwindow." + (en ? "MOCK_DATA_EN" : "MOCK_DATA") + " = " + sanitize(JSON.stringify(data, null, 1)) + ";\n");
console.log("écrit :", out);
