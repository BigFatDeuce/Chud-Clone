// Chud Clone — interface logic.
// The interface decides nothing dangerous: it shows what the engine analysed and
// passes the user's choices on. Every protection is enforced again by the engine.
"use strict";

const T = window.__TAURI__;
const api = T
  ? {
      invoke: T.core.invoke,
      listen: T.event.listen,
      open: (o) => T.dialog.open(o),
      save: (o) => T.dialog.save(o),
      win: () => T.window.getCurrentWindow(),
    }
  : window.__MOCK__;

const $ = (s, r) => (r || document).querySelector(s);
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
const store = {
  get(k, d) { try { return Object.assign({}, d, JSON.parse(localStorage.getItem("chudclone." + k) || "{}")); } catch { return d; } },
  set(k, v) { try { localStorage.setItem("chudclone." + k, JSON.stringify(v)); } catch { /* storage unavailable */ } },
};

// ----------------------------------------------------------------- Language
// Two languages: French and English. `L("texte", "text")` returns the text of the active
// language; the label tables are rebuilt by applyLang().
let LANG = "fr";
let langReady = Promise.resolve();
const L = (fr, en) => (LANG === "en" ? en : fr);
const LOCALE = () => (LANG === "en" ? "en-US" : "fr-FR");
const dec = (x) => (LANG === "en" ? x : x.replace(".", ","));

// ------------------------------------------------------------------ Formats
function size(b) {
  if (b == null) return "—";
  const u = LANG === "en" ? ["B", "KB", "MB", "GB", "TB"] : ["o", "Ko", "Mo", "Go", "To"];
  let v = Number(b), i = 0;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  if (i === 0) return `${b} ${u[0]}`;
  return dec(`${v >= 100 ? v.toFixed(0) : v >= 10 ? v.toFixed(1) : v.toFixed(2)} ${u[i]}`);
}
function dur(sec) {
  if (sec == null) return "—";
  sec = Math.max(0, Math.round(sec));
  const h = Math.floor(sec / 3600), m = Math.floor((sec % 3600) / 60), s = sec % 60;
  if (h) return `${h} h ${String(m).padStart(2, "0")} min`;
  if (m) return `${m} min ${String(s).padStart(2, "0")} s`;
  return `${s} s`;
}
const num = (n) => Number(n || 0).toLocaleString(LOCALE());

let MEDIA, TABLE, SEV, STATUS, PHASES, PHASE_LABEL, METHOD, MODE, HEALTH, BOOT, SCAN_LABELS;
function buildTables() {
  MEDIA = { ssd: "SSD", hdd: L("Disque dur", "Hard disk"), virtual: L("Disque virtuel", "Virtual disk"), image: L("Fichier image", "Image file"), unknown: L("Type inconnu", "Unknown type") };
  TABLE = { gpt: "GPT", mbr: "MBR", none: L("Sans table", "No table") };
  SEV = { info: ["i", "Information"], warning: ["!", L("Avertissement", "Warning")], error: ["×", L("Erreur", "Error")], critical: ["‼", L("Critique", "Critical")] };
  STATUS = {
    success_verified: ["ok", L("Clonage réussi et vérifié", "Clone succeeded and verified")],
    completed_with_warnings: ["warn", L("Clonage terminé avec avertissements", "Clone completed with warnings")],
    incomplete: ["incomplete", L("Clonage incomplet", "Clone incomplete")],
    failed: ["failed", L("Clonage échoué", "Clone failed")],
  };
  PHASES = [
    ["preparing", L("Préparation", "Preparing")],
    ["copying", L("Copie", "Copying")],
    ["verifying", L("Vérification", "Verifying")],
    ["growing", L("Agrandissement", "Growing")],
    ["shrinking", L("Réduction", "Shrinking")],
    ["writing_table", L("Table de partitions", "Partition table")],
    ["checking", L("Contrôle final", "Final check")],
  ];
  PHASE_LABEL = {
    preparing: L("Préparation de la destination", "Preparing the destination"),
    copying: L("Copie des données", "Copying data"),
    recovering: L("Nouvelle lecture des zones difficiles", "Re-reading difficult areas"),
    writing_table: L("Écriture de la table de partitions", "Writing the partition table"),
    verifying: L("Vérification des données copiées", "Verifying copied data"),
    growing: L("Agrandissement de la partition Windows", "Growing the Windows partition"),
    shrinking: L("Réduction de la partition Windows", "Shrinking the Windows partition"),
    checking: L("Contrôle des structures du clone", "Checking the clone's structures"),
    done: L("Terminé", "Done"),
  };
  METHOD = { smart: L("Blocs utilisés", "Used blocks"), raw: L("Secteur par secteur", "Sector by sector"), skip: L("Non copiée", "Not copied") };
  MODE = { smart: L("Intelligent (blocs utilisés)", "Smart (used blocks)"), raw_partitions: L("Partitions complètes (secteur par secteur)", "Whole partitions (sector by sector)"), raw_disk: L("Disque entier (secteur par secteur)", "Whole disk (sector by sector)") };
  HEALTH = { good: L("Aucune anomalie signalée", "No anomaly reported"), watch: L("À surveiller", "Keep an eye on it"), bad: L("Dégradé", "Degraded"), unknown: L("Santé inconnue", "Health unknown") };
  BOOT = { windows_uefi: "Windows (UEFI / GPT)", windows_bios: L("Windows (BIOS hérité / MBR)", "Windows (legacy BIOS / MBR)"), other_uefi: L("Autre système (UEFI)", "Other system (UEFI)"), data_only: L("Disque de données", "Data disk"), unknown: L("Indéterminé", "Undetermined") };
  SCAN_LABELS = ["< 5 ms", "5–20 ms", "20–50 ms", "50–150 ms", "150–500 ms", "≥ 500 ms", L("Illisible", "Unreadable")];
}
buildTables();
const ROLE_CLASS = { esp: "p-esp", msr: "p-msr", basic_data: "p-data", recovery: "p-rec", linux_fs: "p-linux", linux_swap: "p-linux", linux_lvm: "p-linux" };

// -------------------------------------------------------------------- State
const S = {
  env: null,
  disks: [],
  loadingDisks: true,
  view: "clone",
  srcId: null,
  dstId: null,
  // Several destinations can be cloned one after another from the same source.
  dstIds: [],
  preps: [],
  prepIdx: 0,
  queue: null,
  resultIdx: 0,
  shutdown: false,
  fmt: { id: null, fs: "ntfs", table: "gpt", label: "", check: null, confirm: "", steps: [], outcome: null, error: null },
  prep: null,
  prepBusy: false,
  prepError: null,
  confirmText: "",
  opts: store.get("opts", { mode: "smart", optimize_layout: true, grow_main: true, verify: "hash", unreadable: "ask", read_retries: 3, eject: false }),
  ui: store.get("ui", { motion: "full", lang: null, autoUpdate: true, skipVersion: null, sound: true, keepAwake: true }),
  update: { info: null, error: null, checkedAt: null, get checking() { return M.update.is("checking"); }, get installing() { return M.update.is("installing"); } },
  run: null,
  report: null,
  runError: null,
  reports: null,
  reportOpen: null,
  diskSel: null,
  inspect: {},
  fix: {},
  smart: {},
  health: {},
  busy: {},
};
let beam = null, star = null;

// ------------------------------------------------------------ State machines
// The states live in machines.js; this file only asks `can()` and moves with `to()`.
const M = window.Machines;
// The wizard step is the state of the clone machine (read-only here).
Object.defineProperty(S, "step", { get: () => M.clone.state, enumerable: true });

/** What the interface is busy with: one thing at a time, like the engine. */
function activity() {
  if (M.clone.is("run")) return "cloning";
  if (M.format.is("running")) return "formatting";
  if (M.scan.is("running", "cancelling")) return "scanning";
  if (M.update.is("installing")) return "updating";
  if (Object.values(S.fix).some((f) => f.busy)) return "repairing_boot";
  return "idle";
}
const BUSY = () => ({
  cloning: L("Un clonage est en cours.", "A clone is running."),
  formatting: L("Un formatage est en cours.", "A format is running."),
  scanning: L("Un test de surface est en cours : arrêtez-le d'abord.", "A surface scan is running: stop it first."),
  updating: L("Une mise à jour est en cours d'installation.", "An update is being installed."),
  repairing_boot: L("Une réparation du démarrage est en cours.", "A boot repair is running."),
});
/** True (with a message) when something else is running. */
function busy() {
  const a = activity();
  if (a === "idle") return false;
  toast(BUSY()[a], true);
  return true;
}
/** State of one destination's analysis: "busy", "error", "blocked" or "ready". */
const prepState = (x) => (!x || x.busy ? "busy" : x.error ? "error" : x.prep?.blocked ? "blocked" : x.prep ? "ready" : "busy");
/** State of one clone of the queue: "waiting", "running", "ok" or "failed". */
const itemState = (it, i) => (it.done ? (itemGood(it) ? "ok" : "failed") : S.queue && i === S.queue.idx && M.clone.is("run") ? "running" : "waiting");
function newScan(id, cells) {
  return { id, cells, progress: null, result: null, error: null, cursor: 0, get running() { return M.scan.is("running", "cancelling"); }, get cancelling() { return M.scan.is("cancelling"); } };
}
/** Everything a test or a tool needs to know where the application is. */
window.ChudState = () => ({
  view: S.view,
  activity: activity(),
  ...M.snapshot(),
  queue: S.queue ? { index: S.queue.idx, items: S.queue.items.map((it, i) => ({ destination: it.id, state: itemState(it, i) })) } : null,
  destinations: S.dstIds.map((id, i) => ({ id, state: prepState(S.preps[i]) })),
});

const disk = (id) => S.disks.find((d) => d.id === id);
const src = () => disk(S.srcId);
const dst = () => disk(S.dstId);

function toast(msg, err) {
  const t = document.createElement("div");
  t.className = "toast" + (err ? " err" : "");
  t.textContent = msg;
  $("#toasts").appendChild(t);
  setTimeout(() => t.remove(), err ? 9000 : 4500);
}
const fail = (e) => toast(typeof e === "string" ? e : e?.message || String(e), true);

function modal(html) {
  $("#modal-root").innerHTML = `<div class="overlay"><div class="modal" role="dialog" aria-modal="true">${html}</div></div>`;
}
function closeModal() { $("#modal-root").innerHTML = ""; }

// --------------------------------------------------------------- Composants
function strip(parts, total, big) {
  if (!parts.length || !total) return `<div class="strip${big ? " big" : ""}"><i class="free" style="flex:1"></i></div>`;
  let cursor = 0, out = "";
  const sorted = [...parts].sort((a, b) => a.offset - b.offset);
  for (const p of sorted) {
    if (p.offset - cursor > total * 0.004) out += `<i class="free" style="flex:${(p.offset - cursor) / total}"></i>`;
    const used = p.used_bytes != null && p.size ? Math.min(1, p.used_bytes / p.size) : null;
    const label = big && p.size / total > 0.07 ? esc(p.short || p.fs || "") : "";
    out += `<i class="${ROLE_CLASS[p.role] || "p-other"}" style="flex:${Math.max(p.size / total, 0.006)}" title="${esc(p.title || p.type_label)} — ${size(p.size)}">${label}${used != null && !big ? `<u style="width:${used * 100}%"></u>` : ""}</i>`;
    cursor = p.offset + p.size;
  }
  if (total - cursor > total * 0.004) out += `<i class="free" style="flex:${(total - cursor) / total}"></i>`;
  return `<div class="strip${big ? " big" : ""}">${out}</div>`;
}

function diskChips(d, extra) {
  const c = [];
  const busCls = d.kind === "image" ? "file" : { nvme: "nvme", sata: "sata", ata: "sata", usb: "usb" }[String(d.bus).toLowerCase()] || "";
  c.push(`<span class="chip ${busCls}">${esc(d.bus)}</span>`);
  if (d.media !== "unknown" && !(d.kind === "image" && d.media === "image")) c.push(`<span class="chip">${MEDIA[d.media] || esc(d.media)}</span>`);
  c.push(`<span class="chip">${TABLE[d.table]}</span>`);
  if (d.is_system) c.push(`<span class="chip sys">${L("Windows en cours", "Running Windows")}</span>`);
  if (d.removable) c.push(`<span class="chip rem">${L("Amovible", "Removable")}</span>`);
  if (d.read_only) c.push(`<span class="chip warn">${L("Lecture seule", "Read-only")}</span>`);
  if (d.offline) c.push(`<span class="chip warn">${L("Hors ligne", "Offline")}</span>`);
  const sm = S.smart[d.id];
  if (sm?.available) c.push(`<span class="chip ${sm.level === "good" ? "ok" : sm.level === "watch" ? "warn" : "bad"}">${L("Santé : ", "Health: ")}${HEALTH[sm.level].toLowerCase()}</span>`);
  return c.join("") + (extra || "");
}

function diskCard(d, o) {
  o = o || {};
  const used = d.used_bytes;
  const mounts = d.partitions.flatMap((p) => p.mounts).map((m) => `<span class="drv">${esc(m.replace(/\\$/, ""))}</span>`).join(" ");
  const sub = [d.number != null ? `${L("Disque", "Disk")} ${d.number}` : L("Fichier image", "Image file"), mounts].filter(Boolean).join(" · ");
  const cls = ["disk", o.mini ? "mini" : "", o.selected ? "selected" : "", o.dest ? "dest" : "", o.locked ? "locked" : ""].join(" ");
  const tag = o.tag ? `<span class="role-tag${o.dest ? " dest" : ""}">${o.tag}</span>` : "";
  const inner = `${tag}<div class="planet">${Cosmos.planet(d)}</div><div style="min-width:0">
      <h3 title="${esc(d.model)}">${esc(d.model || L("Disque sans nom", "Unnamed disk"))}</h3>
      <div class="sub">${sub}${o.mini && S.smart[d.id]?.available ? ` <span class="hdot ${S.smart[d.id].level}" title="${HEALTH[S.smart[d.id].level]}"></span>${S.health[d.id]?.trends.some((t) => t.severity === "error" || t.code === "TREND_REALLOCATED" || t.code === "TREND_SPARE") ?` <b style="color:var(--warn)">${L("en dégradation", "degrading")}</b>` : ""}` : ""}</div>
      ${o.mini ? "" : `<div class="cap"><b>${size(d.size)}</b><span>${used != null ? `${size(used)} ${L("utilisés", "used")}` : d.partitions.length ? L("occupation inconnue", "usage unknown") : L("aucune partition", "no partition")}</span></div>
      ${strip(d.partitions, d.size)}
      <div class="chips">${diskChips(d, o.chips)}</div>${o.note ? `<div class="sub" style="margin-top:7px;color:var(--warn)">${esc(o.note)}</div>` : ""}`}
    </div>`;
  return o.act ? `<button class="${cls}" data-act="${o.act}" data-id="${esc(d.id)}" ${o.locked ? "disabled" : ""}>${inner}</button>` : `<div class="${cls}">${inner}</div>`;
}

function findingsHtml(list) {
  if (!list || !list.length) return `<p class="muted">${L("Aucun constat.", "No findings.")}</p>`;
  return `<div class="findings">${list
    .map((f) => `<div class="finding ${f.severity}"><div class="ico">${SEV[f.severity][0]}</div><div><span class="tag">${SEV[f.severity][1]}</span><b>${esc(f.title)}</b>${f.scope ? ` <span class="dim">· ${esc(f.scope)}</span>` : ""}${f.blocking ? ` <span class="block">· ${L("bloquant", "blocking")}</span>` : ""}<p class="selectable">${esc(f.detail)}</p></div></div>`)
    .join("")}</div>`;
}

function flight() {
  const steps = [["source", "Source"], ["destination", "Destination"], ["review", L("Vérification", "Review")], ["run", L("Clonage", "Cloning")], ["result", L("Rapport", "Report")]];
  const cur = steps.findIndex((s) => s[0] === S.step);
  return `<div class="flight">${steps
    .map((s, i) => `${i ? `<div class="leg ${i <= cur ? "done" : ""} ${i === cur + 1 ? "active" : ""}"></div>` : ""}<div class="node ${i < cur ? "done" : i === cur ? "current" : ""}"><span class="orb">${i < cur ? "✓" : i + 1}</span>${s[1]}</div>`)
    .join("")}</div>`;
}

function kv(rows) {
  return `<dl class="kv selectable">${rows.filter(Boolean).map(([k, v]) => `<dt>${k}</dt><dd>${v}</dd>`).join("")}</dl>`;
}
function identity(d, before) {
  return kv([
    [L("Modèle", "Model"), esc(d.model || "—")],
    d.kind === "image" ? null : [L("N° de série", "Serial number"), esc(d.serial || L("non communiqué", "not reported"))],
    [L("Capacité", "Capacity"), `${size(d.size)} <span class="dim">(${num(d.size)} ${L("octets", "bytes")})</span>`],
    ["Interface", d.kind === "image" ? L("Fichier image", "Image file") : `${esc(d.bus)} · ${MEDIA[d.media]}`],
    [L("Secteurs", "Sectors"), L(`${d.sector_size} octets logiques / ${d.physical_sector_size} physiques`, `${d.sector_size} bytes logical / ${d.physical_sector_size} physical`)],
    [before ? L("Partitionnement avant clonage", "Partitioning before cloning") : L("Partitionnement", "Partitioning"), TABLE[d.table]],
    [L("Emplacement", "Location"), `<code>${esc(d.path)}</code>`],
  ]);
}

// ------------------------------------------------------------- Vue : Cloner
function viewClone() {
  if (S.step === "run") return viewRun();
  if (S.step === "result") return `<div class="page">${flight()}${queueSummary()}${outcomeHtml(S.report, S.runError)}</div>`;
  if (S.step === "review") return viewReview();
  const isSrc = S.step === "source";
  const s = src();
  const head = isSrc
    ? [L("Source", "Source"), L("Disque à copier. Lecture seule.", "Disk to copy. Read-only.")]
    : [L("Destination(s)", "Destination(s)"), L("Contenu effacé. Plusieurs disques possibles : clonés l'un après l'autre.", "Content erased. Several disks allowed: cloned one after another.")];
  let cards;
  if (S.loadingDisks) cards = `<div class="empty"><div class="orbit-loader"></div>${L("Recherche des disques…", "Looking for disks…")}</div>`;
  else {
    cards = S.disks
      .map((d) => {
        if (isSrc) return diskCard(d, { act: "pick", selected: d.id === S.srcId, tag: d.id === S.srcId ? "Source" : "", note: d.is_system ? L("Windows tourne sur ce disque : il ne peut être cloné que depuis un autre Windows ou un environnement de secours.", "Windows is running from this disk: it can only be cloned from another Windows or a rescue environment.") : "" });
        if (d.id === S.srcId) return diskCard(d, { act: "pick", locked: true, selected: true, tag: "Source" });
        let chips = "", note = "", locked = false;
        if (d.is_system) { locked = true; note = L("Contient le Windows en cours d'exécution : ne peut pas être effacé.", "Holds the running Windows: cannot be erased."); }
        else if (d.hosts_app) { locked = true; note = L("Contient Chud Clone ou ses rapports : ne peut pas être effacé.", "Holds Chud Clone or its reports: cannot be erased."); }
        else if (d.read_only) { locked = true; note = L("Protégé en écriture.", "Write-protected."); }
        else if (s) {
          if (d.size >= s.size) chips = `<span class="chip ok">${L("Capacité suffisante", "Large enough")}</span>`;
          else if (s.used_bytes != null && d.size > s.used_bytes * 1.05) chips = `<span class="chip warn">${L("Plus petit que la source", "Smaller than the source")}</span>`;
          else chips = `<span class="chip bad">${L("Probablement trop petit", "Probably too small")}</span>`;
        }
        const qi = S.dstIds.indexOf(d.id);
        return diskCard(d, { act: "pick", selected: qi >= 0, dest: true, tag: qi >= 0 ? (S.dstIds.length > 1 ? `Destination ${qi + 1}` : "Destination") : "", chips, note, locked });
      })
      .join("");
    cards = `<div class="disk-grid">${cards}
      <button class="addcard" data-act="add-image"><div><b>${L("Ajouter un fichier image…", "Add an image file…")}</b><br><span class="dim">${L("Utiliser une image disque existante comme", "Use an existing disk image as the")} ${isSrc ? "source" : "destination"}</span></div></button>
      ${isSrc ? "" : `<button class="addcard" data-act="new-image" ${s ? "" : "disabled"}><div><b>${L("Créer une image de destination…", "Create a destination image…")}</b><br><span class="dim">${L("Sauvegarder le disque source dans un fichier", "Save the source disk to a file")}</span></div></button>`}
    </div>`;
  }
  const limited = S.env && !S.env.elevated
    ? `<div class="finding warning" style="margin-bottom:14px"><div class="ico">!</div><div><b>${L("Mode limité (sans droits administrateur)", "Limited mode (no administrator rights)")}</b><p>${L("Les disques sont listés d'après ce que Windows expose. Pour analyser et cloner un disque réel, Chud Clone doit lire les disques directement : cliquez sur « Passer en administrateur » en haut à droite. Les fichiers image fonctionnent sans cela.", "Disks are listed from what Windows exposes. To analyse and clone a real disk, Chud Clone must read disks directly: click \"Switch to administrator\" at the top right. Image files work without it.")}</p></div></div>`
    : "";
  const chosen = isSrc ? s : S.dstIds.length ? disk(S.dstIds[0]) : null;
  const chosenTxt = !chosen ? L("Aucun disque sélectionné", "No disk selected")
    : isSrc || S.dstIds.length === 1 ? `${isSrc ? "Source" : "Destination"}${L(" :", ":")} <b style="color:var(--text)">${esc(chosen.model)}</b> · ${size(chosen.size)}`
    : `<b style="color:var(--text)">${S.dstIds.length} destinations</b> · ${L("clonées l'une après l'autre", "cloned one after another")}`;
  return `<div class="page">${flight()}
    <div class="page-head"><div><h1>${head[0]}</h1><p>${head[1]}</p></div><button class="btn small ghost" data-act="refresh">↻ ${L("Actualiser", "Refresh")}</button></div>
    ${limited}${cards}
    <div class="footer-bar">
      ${isSrc ? "" : `<button class="btn" data-act="back">← ${L("Retour", "Back")}</button>`}
      <span class="muted">${chosenTxt}</span>
      <span class="spacer"></span>
      <button class="btn primary" data-act="next" ${chosen ? "" : "disabled"}>${isSrc ? L("Choisir la destination →", "Choose the destination →") : L("Vérifier avant de cloner →", "Review before cloning →")}</button>
    </div></div>`;
}

function planStrips(prep) {
  const p = prep.plan;
  const mk = (which) => p.partitions.map((x) => ({ offset: which === "src" ? x.src_start : x.dst_start, size: which === "src" ? x.src_len : x.dst_len, role: x.role, type_label: x.type_label, short: x.fs_label || (x.fs !== "unknown" && x.fs !== "blank" ? x.fs.toUpperCase() : ""), title: `Partition ${x.number} — ${x.type_label}` }));
  return `<div class="muted" style="margin-bottom:5px">Source — ${size(p.src_size)}</div>${strip(mk("src"), p.src_size, true)}
    <div class="muted" style="margin:12px 0 5px">${L("Destination après clonage", "Destination after cloning")} — ${size(p.dst_size)}</div>${strip(mk("dst"), p.dst_size, true)}
    <div class="legend"><span style="--c:#4fe3ff">${L("Démarrage EFI", "EFI boot")}</span><span style="--c:#3a7bff">${L("Données / Windows", "Data / Windows")}</span><span style="--c:#9a6bff">${L("Récupération", "Recovery")}</span><span style="--c:#5a6699">${L("Réservée", "Reserved")}</span><span style="--c:#8892b8">${L("Autre", "Other")}</span><span style="--c:rgba(255,255,255,.18)">${L("Non alloué", "Unallocated")}</span></div>`;
}

function smartLine(label, sm) {
  if (!sm) return "";
  if (!sm.available) return `<div class="gauge"><span class="lv unknown"></span><div><b>${label}</b> <span class="muted">— ${L("santé non lue : ", "health not read: ")}${esc(sm.unavailable_reason || "")}</span></div></div>`;
  const bits = [sm.temperature_c != null ? `${sm.temperature_c} °C` : "", sm.power_on_hours != null ? `${num(sm.power_on_hours)} ${L("h de fonctionnement", "power-on hours")}` : "", sm.wear_percent_used != null ? `${L("usure estimée", "estimated wear")} ${sm.wear_percent_used} %` : ""].filter(Boolean).join(" · ");
  return `<div class="gauge"><span class="lv ${sm.level}"></span><div><b>${label}</b> — ${HEALTH[sm.level]} <span class="muted">${bits ? "· " + bits : ""}</span></div></div>`;
}

function optionsHtml() {
  const o = S.opts;
  const sel = (k, list) => `<select data-opt="${k}">${list.map(([v, l]) => `<option value="${v}" ${String(o[k]) === String(v) ? "selected" : ""}>${l}</option>`).join("")}</select>`;
  return `<details class="adv" ${S.advOpen ? "open" : ""} data-keep="adv"><summary>${L("Options avancées", "Advanced options")}</summary><div class="body">
    <div class="opt"><div><label>${L("Méthode de clonage", "Clone method")}</label><small>${L("« Intelligent » ne copie que les blocs utilisés quand c'est sûr, et se replie seul sur la copie intégrale sinon.", "\"Smart\" copies only the used blocks when it is safe, and falls back on its own to a full copy otherwise.")}</small></div>${sel("mode", [["smart", MODE.smart + L(" — recommandé", " — recommended")], ["raw_partitions", MODE.raw_partitions], ["raw_disk", MODE.raw_disk]])}</div>
    <div class="opt"><div><label>${L("Vérification après copie", "Verification after the copy")}</label><small>${L("« Complète » relit aussi la source : plus long, détecte une source instable.", "\"Full\" also re-reads the source: slower, detects an unstable source.")}</small></div>${sel("verify", [["hash", L("Relecture de la destination — recommandé", "Read the destination back — recommended")], ["full", L("Comparaison complète source / destination", "Full source / destination comparison")], ["none", L("Aucune (le résultat ne sera jamais « vérifié »)", "None (the result will never be \"verified\")")]])}</div>
    <div class="opt"><div><label>${L("Secteurs illisibles sur la source", "Unreadable sectors on the source")}</label><small>${L("Par défaut, rien n'est ignoré en silence : vous décidez au moment où le cas se présente.", "By default nothing is silently skipped: you decide when it happens.")}</small></div>${sel("unreadable", [["ask", L("Me demander — recommandé", "Ask me — recommended")], ["stop", L("Toujours arrêter le clonage", "Always stop cloning")], ["zero_fill", L("Poursuivre en remplaçant par des zéros (données perdues)", "Continue, replacing with zeros (data lost)")]])}</div>
    <div class="opt"><div><label>${L("Nouvelles tentatives par secteur", "Retries per sector")}</label><small>${L("Insister davantage peut aggraver un disque mourant.", "Insisting more can make a dying disk worse.")}</small></div>${sel("read_retries", [[0, "0"], [1, "1"], [3, L("3 — recommandé", "3 — recommended")], [8, "8"], [20, "20"]])}</div>
    <div class="opt"><div><label>${L("Préparer l'extension (disque plus grand)", "Prepare for extension (larger disk)")}</label><small>${L("Place la partition de récupération en fin de disque pour que l'espace libre soit contigu à la partition principale.", "Places the recovery partition at the end of the disk so that the free space is contiguous with the main partition.")}</small></div><label class="toggle"><input type="checkbox" data-opt="optimize_layout" ${o.optimize_layout ? "checked" : ""}><span>${L("Activé", "Enabled")}</span></label></div>
    <div class="opt"><div><label>${L("Ajuster la partition Windows", "Resize the Windows partition")}</label><small>${L("Sur un disque plus grand, étend le volume NTFS principal sur l'espace ajouté. Sur un disque plus petit que les partitions de la source, le réduit pour qu'il tienne (les données situées au-delà de la nouvelle fin sont recopiées plus bas ; la source n'est pas modifiée). Windows contrôlera le volume une fois (chkdsk) au premier démarrage.", "On a larger disk, extends the main NTFS volume over the added space. On a disk smaller than the source's partitions, shrinks it so that it fits (the data beyond the new end is copied lower down; the source is not modified). Windows will check the volume once (chkdsk) on first boot.")}</small></div><label class="toggle"><input type="checkbox" data-opt="grow_main" ${o.grow_main !== false ? "checked" : ""}><span>${L("Activé", "Enabled")}</span></label></div>
    <div class="opt"><div><label>${L("Préparer le démarrage pour un autre PC", "Prepare boot for another PC")}</label><small>${L("Clone de Windows : la configuration de démarrage désigne les partitions du clone, les pilotes NVMe et AHCI démarrent avec Windows et une session en veille prolongée est annulée. Évite « Votre PC doit être réparé » sur le nouveau PC. La source n'est pas modifiée.", "Windows clone: the boot configuration designates the clone's partitions, the NVMe and AHCI drivers start with Windows and a hibernated session is cancelled. Avoids \"Your PC needs to be repaired\" on the new PC. The source is not modified.")}</small></div><label class="toggle"><input type="checkbox" data-opt="boot_prep" ${o.boot_prep !== false ? "checked" : ""}><span>${L("Activé", "Enabled")}</span></label></div>
  </div></details>`;
}

function viewReview() {
  const s = src(), d = dst();
  let body;
  if (S.prepBusy) body = `<div class="panel empty"><div class="orbit-loader"></div><b>${L("Analyse en cours…", "Analysing…")}</b><p class="muted">${L("Lecture de la table de partitions et des systèmes de fichiers de la source, état de santé des disques. Rien n'est écrit.", "Reading the source's partition table and file systems, and the disks' health. Nothing is written.")}</p></div>`;
  else if (S.prepError) {
    const needAdmin = S.env && !S.env.elevated && /administra/i.test(S.prepError);
    body = `<div class="panel"><div class="finding error"><div class="ico">×</div><div><b>${L("L'analyse n'a pas pu être faite", "The analysis could not be done")}</b><p class="selectable">${esc(S.prepError)}</p></div></div>
      <div class="row" style="margin-top:14px">${needAdmin ? `<button class="btn primary" data-act="elevate">${L("Passer en administrateur", "Switch to administrator")}</button>` : ""}<button class="btn" data-act="reprepare">${L("Réessayer", "Try again")}</button></div></div>`;
  } else if (S.prep) {
    const p = S.prep, plan = p.plan;
    const blocking = p.findings.filter((f) => f.blocking);
    const others = p.findings.filter((f) => !f.blocking);
    const readyList = S.preps.filter((x) => prepState(x) === "ready");
    const ready = readyList.length > 0;
    const word = confirmWord();
    const good = S.confirmText.trim().toUpperCase() === word;
    // A serious problem (degraded source disk…) is raised to the top of the page.
    const urgent = others.some((f) => f.severity === "error" || f.severity === "critical");
    const attention = `<div class="panel"><h2>${L("Points d'attention", "Points of attention")}</h2>${findingsHtml(others)}</div>`;
    body = `
      <div class="panel"><div class="route">
        <div class="end src"><div class="planet">${Cosmos.planet(p.source)}</div><div><div class="lbl">${L("Source — sera lue", "Source — will be read")}</div><h3>${esc(p.source.model)}</h3><div class="muted">${size(p.source.size)} · ${esc(p.source.bus)} · ${L("n°", "S/N")} ${esc(p.source.serial || "—")}</div></div></div>
        <div class="arrow"></div>
        <div class="end dst"><div class="planet">${Cosmos.planet(p.destination)}</div><div><div class="lbl">${L("Destination — sera effacée", "Destination — will be erased")}</div><h3>${esc(p.destination.model)}</h3><div class="muted">${size(p.destination.size)} · ${esc(p.destination.bus)} · ${L("n°", "S/N")} ${esc(p.destination.serial || "—")}</div></div></div>
      </div></div>
      ${blocking.length ? `<div class="panel" style="border-color:rgba(255,79,130,.5)"><h2 style="color:var(--danger)">${L("Clonage impossible en l'état", "Cloning is not possible as things stand")}</h2>${findingsHtml(blocking)}</div>` : ""}
      ${urgent ? attention : ""}
      <div class="panel"><h2>${L("Ce qui sera fait", "What will be done")}</h2>
        ${planStrips(p)}
        <div class="row wrap" style="margin-top:14px;gap:22px">
          <div><div class="dim">${L("Méthode", "Method")}</div><b>${MODE[plan.mode]}</b></div>
          <div><div class="dim">${L("À copier", "To copy")}</div><b>${size(plan.total_copy_bytes)}</b> <span class="muted">${L("sur", "of")} ${size(plan.src_size)}</span></div>
          <div><div class="dim">${L("Type de disque", "Disk type")}</div><b>${esc(BOOT[p.boot.style] || "—")}</b></div>
        </div>
        <table class="t" style="margin-top:12px"><tr><th>${L("N°", "No.")}</th><th>Partition</th><th>${L("Système", "File system")}</th><th>${L("Taille", "Size")}</th><th>${L("Utilisé", "Used")}</th><th>${L("Copie", "Copy")}</th><th>${L("Emplacement", "Position")}</th></tr>
        ${plan.partitions.map((x) => `<tr><td>${x.number}</td><td>${esc(x.type_label)}${x.fs_label ? ` <span class="dim">${L(`« ${esc(x.fs_label)} »`, `“${esc(x.fs_label)}”`)}</span>` : ""}</td><td>${esc(FS[x.fs] || x.fs)}</td><td class="num">${size(x.src_len)}</td><td class="num">${x.used_bytes != null ? size(x.used_bytes) : "—"}</td><td class="${{ smart: "okc", raw: "warnc", skip: "dim" }[x.method] || ""}">${METHOD[x.method]}${x.method === "raw" && x.method_reason ? `<div class="dim">${esc(x.method_reason)}</div>` : ""}</td><td>${x.moved ? L("déplacée", "moved") : L("inchangée", "unchanged")}${x.dst_len > x.src_len ? `<div class="grow-tag">${L("agrandie à", "grown to")} ${size(x.dst_len)}</div>` : ""}${x.dst_len < x.src_len ? `<div class="grow-tag">${L("réduite à", "shrunk to")} ${size(x.dst_len)}</div>` : ""}</td></tr>`).join("")}
        </table>
      </div>
      <div class="cols" style="margin-top:14px">
        <div class="panel" style="margin:0"><h2>${L("Santé des disques", "Disk health")}</h2><div style="display:grid;gap:10px">${smartLine("Source", p.smart_source)}${smartLine("Destination", p.smart_destination)}</div><p class="dim" style="margin-top:10px">${L("Indicateurs fournis par les disques eux-mêmes : une estimation, pas une certitude.", "Indicators reported by the disks themselves: an estimate, not a certainty.")}</p></div>
        <div class="panel" style="margin:0"><h2>${L("Démarrage de la source", "Source boot")}</h2>${p.boot.checks.length ? `<div class="checks">${p.boot.checks.map(checkRow).join("")}</div>` : `<p class="muted">${L("Aucun élément de démarrage : disque de données.", "No boot elements: data disk.")}</p>`}</div>
      </div>
      ${urgent ? "" : attention}
      ${optionsHtml()}
      ${ready ? `<div class="danger-zone" style="margin-top:14px"><h2 class="dz-title">${L("Irréversible", "Irreversible")}</h2>
        <ul class="erase-list">${readyList.map((x) => `<li><b>${esc(x.prep.destination.model)}</b> <span class="mono dim">${x.prep.destination.number != null ? "disk " + x.prep.destination.number : ""} · ${size(x.prep.destination.size)}</span> — ${L("effacé", "erased")}</li>`).join("")}</ul>
        ${readyList.length < S.preps.length ? `<p class="muted">${L(`${S.preps.length - readyList.length} destination(s) bloquée(s) : ignorée(s).`, `${S.preps.length - readyList.length} blocked destination(s): skipped.`)}</p>` : ""}
        <div class="row wrap end-opts">
          <label class="toggle"><input type="checkbox" data-opt="eject" ${S.opts.eject ? "checked" : ""}><span>${L("Éjecter la destination à la fin", "Eject the destination when done")}</span></label>
          <label class="toggle"><input type="checkbox" id="opt-shutdown" ${S.shutdown ? "checked" : ""}><span>${L("Éteindre le PC à la fin", "Shut down the PC when done")}</span></label>
        </div>
        <div class="row wrap" style="margin-top:12px"><span>${L("Saisir", "Type")} <b class="mono">${word}</b> ${L("puis Entrée", "then Enter")}</span><input type="text" id="confirm" class="${good ? "good" : ""}" value="${esc(S.confirmText)}" autocomplete="off" spellcheck="false" maxlength="12"><span class="spacer"></span><button class="btn danger" data-act="start" ${good ? "" : "disabled"}>${readyList.length > 1 ? L(`Lancer ${readyList.length} clonages`, `Start ${readyList.length} clones`) : L("Lancer le clonage", "Start cloning")}</button></div></div>` : ""}`;
  } else body = "";
  const tabs = S.dstIds.length > 1 ? `<div class="qtabs">${S.dstIds.map((id, i) => {
    const x = S.preps[i] || {}, dd = disk(id);
    const st = { busy: ["", "…"], error: ["bad", L("erreur", "error")], blocked: ["bad", L("bloqué", "blocked")], ready: ["ok", L("prêt", "ready")] }[prepState(x)];
    return `<button class="qtab ${i === S.prepIdx ? "on" : ""}" data-act="prep-tab" data-i="${i}" data-state="${prepState(x)}"><span class="n">${i + 1}</span>${esc(dd?.model || id)}<span class="chip ${st[0]}">${st[1]}</span></button>`;
  }).join("")}</div>` : "";
  return `<div class="page">${flight()}
    <div class="page-head"><div><h1>${L("Vérification", "Review")}</h1><p class="mono">${s && d ? `${esc(s.model)} → ${S.dstIds.length > 1 ? `${S.dstIds.length} destinations` : esc(d.model)}` : ""}</p></div></div>
    ${tabs}${body}
    <div class="footer-bar"><button class="btn" data-act="back">← ${L("Retour", "Back")}</button><span class="spacer"></span>${S.prep && !S.prepBusy ? `<button class="btn small ghost" data-act="reprepare">↻ ${L("Refaire l'analyse", "Analyse again")}</button>` : ""}</div></div>`;
}
const FS = { ntfs: "NTFS", fat12: "FAT12", fat16: "FAT16", fat32: "FAT32", exfat: "exFAT", bitlocker: "BitLocker", refs: "ReFS", ext: "ext2/3/4", xfs: "XFS", btrfs: "Btrfs", linux_swap: "Swap", luks: "LUKS", blank: "—", unknown: "—" };
function checkRow(c) {
  const m = { present: ["y", "✓"], missing: ["n", "×"], unknown: ["u", "?"] }[c.state];
  return `<div class="check ${m[0]}"><span class="s">${m[1]}</span><span>${esc(c.label)}${c.state === "unknown" ? ` <span class="dim">— ${L("non vérifiable", "not verifiable")}</span>` : c.state === "missing" ? ` <span class="dim">— ${L("absent", "missing")}${c.required ? "" : L(" (facultatif)", " (optional)")}</span>` : ""}</span></div>`;
}

// --------------------------------------------------------- View: clone running
function viewRun() {
  const s = S.run.source, d = S.run.destination;
  const q = S.queue;
  const qbar = q && q.items.length > 1 ? `<div class="qtabs">${q.items.map((it, i) => `<span class="qtab ${i === q.idx ? "on" : ""}" data-state="${itemState(it, i)}"><span class="n">${i + 1}</span>${esc(it.prep.destination.model)}<span class="chip ${{ ok: "ok", failed: "bad" }[itemState(it, i)] || ""}">${{ ok: "OK", failed: L("échec", "failed"), running: L("en cours", "running"), waiting: L("en attente", "waiting") }[itemState(it, i)]}</span></span>`).join("")}</div>` : "";
  return `<div class="page">${flight()}${qbar}
    <div class="scene"><canvas id="beam"></canvas>
      <div class="body src"><div class="planet">${Cosmos.planet(s)}</div><div class="lbl">Source</div><div class="name" title="${esc(s.model)}">${esc(s.model)}</div></div>
      <div class="body dst"><div class="planet" id="dst-planet">${Cosmos.planet(d)}</div><div class="lbl">Destination</div><div class="name" title="${esc(d.model)}">${esc(d.model)}</div></div>
      <div class="ring"><canvas id="star"></canvas><svg viewBox="0 0 100 100"><defs><linearGradient id="ringflow" x1="0" x2="1"><stop offset="0" stop-color="#4fe3ff"/><stop offset=".55" stop-color="#3a7bff"/><stop offset="1" stop-color="#9a6bff"/></linearGradient></defs><circle class="track" cx="50" cy="50" r="44"/><circle class="bar" id="ring-bar" cx="50" cy="50" r="44" stroke-dasharray="276.46" stroke-dashoffset="276.46"/></svg>
        <div class="txt"><div class="pct"><span id="r-pct">0</span><small>%</small></div><div class="ph" id="r-phase">${L("Démarrage…", "Starting…")}</div></div></div>
    </div>
    <div class="phases" id="phases"></div>
    <div class="stats">
      <div class="stat"><div class="k">${L("Débit", "Speed")}</div><div class="v" id="s-speed">—</div></div>
      <div class="stat"><div class="k">${L("Traité", "Processed")}</div><div class="v" id="s-done">—</div></div>
      <div class="stat"><div class="k">${L("Temps écoulé", "Elapsed")}</div><div class="v" id="s-elapsed">0 s</div></div>
      <div class="stat"><div class="k">${L("Temps restant estimé", "Estimated time left")}</div><div class="v" id="s-eta">—</div></div>
      <div class="stat" id="b-rerr"><div class="k">${L("Erreurs de lecture", "Read errors")}</div><div class="v" id="s-rerr">0</div></div>
      <div class="stat" id="b-bad"><div class="k">${L("Illisible / récupéré", "Unreadable / recovered")}</div><div class="v" id="s-bad">0 / 0</div></div>
      <div class="stat" id="b-werr"><div class="k">${L("Erreurs d'écriture", "Write errors")}</div><div class="v" id="s-werr">0</div></div>
      <div class="stat"><div class="k">${L("Position (LBA source)", "Position (source LBA)")}</div><div class="v mono" style="font-size:15px" id="s-lba">—</div></div>
    </div>
    <details class="log" ${S.logOpen ? "open" : ""} data-keep="log"><summary>${L("Journal de l'opération", "Operation log")}</summary><pre id="log"></pre></details>
    <div class="footer-bar"><span class="muted">${L("Ne débranchez aucun disque.", "Do not unplug any disk.")}${q?.shutdown ? ` · <b style="color:var(--warn)">${L("extinction à la fin", "shutdown when done")}</b>` : ""}${q?.eject ? ` · ${L("éjection à la fin", "eject when done")}` : ""}</span><span class="spacer"></span><button class="btn" data-act="cancel" id="btn-cancel">${L("Annuler…", "Cancel…")}</button></div></div>`;
}

function updateRun() {
  const r = S.run;
  if (!r || S.step !== "run" || !$("#r-pct")) return;
  const p = r.progress;
  const phase = p?.phase || "preparing";
  const idx = PHASES.findIndex((x) => x[0] === (phase === "recovering" ? "copying" : phase));
  $("#phases").innerHTML = PHASES.map((x, i) => `<div class="ph ${i < idx || phase === "done" ? "done" : i === idx ? "now" : ""}"><span class="d"></span>${x[1]}${i === 1 && phase === "recovering" ? L(" — zones difficiles", " — difficult areas") : ""}</div>`).join("");
  $("#r-phase").textContent = PHASE_LABEL[phase] || "";
  if (p && p.total > 0) {
    const pct = Math.min(100, (p.done / p.total) * 100);
    $("#r-pct").textContent = pct >= 99.95 ? "100" : dec(pct.toFixed(1));
    $("#ring-bar").style.strokeDashoffset = String(276.46 * (1 - pct / 100));
    $("#s-speed").innerHTML = `${size(p.speed_bps)}<small>/s</small>`;
    $("#s-done").innerHTML = `${size(p.done)}<small>/ ${size(p.total)}</small>`;
    $("#s-eta").textContent = p.eta_secs != null ? "≈ " + dur(p.eta_secs) : L("calcul…", "estimating…");
    $("#s-lba").textContent = num(p.current_lba);
    if (phase === "copying" || phase === "recovering") Object.assign(r.err, { read: p.read_errors, bad: p.bad_bytes, rec: p.recovered_bytes, write: p.write_errors });
  } else if (phase !== "done") {
    $("#s-speed").textContent = "—";
    $("#s-eta").textContent = "—";
  }
  $("#s-elapsed").textContent = dur((Date.now() - r.started) / 1000);
  $("#s-rerr").textContent = num(r.err.read);
  $("#s-bad").innerHTML = `${size(r.err.bad)}<small>/ ${size(r.err.rec)}</small>`;
  $("#s-werr").textContent = num(r.err.write);
  $("#b-rerr").className = "stat" + (r.err.read ? " alert" : "");
  $("#b-bad").className = "stat" + (r.err.bad ? " bad" : r.err.rec ? " alert" : "");
  $("#b-werr").className = "stat" + (r.err.write ? " bad" : "");
  if (star) {
    // The star grows while the data is copied, explodes when the copy is complete,
    // and what is left cools down while the clone is verified and checked.
    const frac = p && p.total > 0 ? Math.min(1, p.done / p.total) : 0;
    if (phase === "copying") r.copied = Math.max(r.copied || 0, frac);
    // Once the result is known, what is left of the star no longer changes.
    if (r.ended) star.set(r.ended === "ok" || r.ended === "warn" ? "dwarf" : "dead", 1, r.ended === "warn");
    else if (phase === "preparing") star.set("proto", 0, false);
    else if (phase === "copying" || phase === "recovering") star.set("grow", r.copied || 0, phase === "recovering" || r.err.read > 0);
    else star.set("remnant", { verifying: 0.02 + 0.8 * frac, growing: 0.88, shrinking: 0.88, writing_table: 0.93, checking: 0.96, done: 1 }[phase] ?? 0.5, false);
  }
  if (beam) {
    const k = p?.speed_bps ? Math.min(1, Math.max(0.12, Math.log10(p.speed_bps / 1e6) / 3)) : 0.25;
    beam.set({ preparing: "prepare", copying: "copy", recovering: "recover", writing_table: "table", verifying: "verify", growing: "table", shrinking: "table", checking: "check", done: "check" }[phase], k, r.err.read > 0);
  }
}

function pushLog(e) {
  const r = S.run;
  if (!r) return;
  r.logs.push(e);
  const pre = $("#log");
  if (!pre) return;
  const cls = e.level === "warning" ? "w" : e.level === "error" ? "e" : "";
  const line = document.createElement("div");
  line.innerHTML = `<span class="t">${esc(e.time)}</span> <span class="${cls}">${esc(e.message)}</span>`;
  const stick = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 30;
  pre.appendChild(line);
  if (stick) pre.scrollTop = pre.scrollHeight;
}

function askUnreadable(info) {
  const rows = info.ranges.slice(0, 12).map((b) => `<tr><td class="mono">${num(b.lba)}${b.sectors > 1 ? " – " + num(b.lba + b.sectors - 1) : ""}</td><td class="num">${num(b.sectors)}</td><td class="num">${b.attempts}</td><td>${esc(b.error)}</td></tr>`).join("");
  modal(`<h2 style="color:var(--warn)">${num(info.total_sectors)} ${L("secteur(s) illisible(s) sur la source", "unreadable sector(s) on the source")}</h2>
    <p>${L(`Tout le reste du disque a été copié. Ces secteurs (${size(info.total_bytes)}) ont résisté à plusieurs tentatives de lecture : le disque source est endommagé à ces endroits.`, `The rest of the disk has been copied. These sectors (${size(info.total_bytes)}) resisted several read attempts: the source disk is damaged at these places.`)}</p>
    <table class="t" style="margin:12px 0"><tr><th>${L("Position (LBA)", "Position (LBA)")}</th><th>${L("Secteurs", "Sectors")}</th><th>${L("Tentatives", "Attempts")}</th><th>${L("Erreur", "Error")}</th></tr>${rows}</table>
    ${info.ranges.length > 12 ? `<p class="dim">${L(`… et ${info.ranges.length - 12} autre(s) plage(s), détaillées dans le rapport.`, `… and ${info.ranges.length - 12} more range(s), detailed in the report.`)}</p>` : ""}
    <div class="finding warning" style="margin-top:10px"><div class="ico">!</div><div><b>${L("Si vous poursuivez", "If you continue")}</b><p>${L("Ces secteurs seront remplacés par des zéros sur le clone. Les fichiers qui les utilisaient seront corrompus ; s'il s'agit de structures du système de fichiers, Windows peut exiger une réparation ou refuser de démarrer. Le résultat sera déclaré « incomplet », avec la liste exacte des positions.", "These sectors will be replaced with zeros on the clone. The files that used them will be corrupted; if they are file system structures, Windows may require a repair or refuse to boot. The result will be reported as \"incomplete\", with the exact list of positions.")}</p></div></div>
    <div class="finding info" style="margin-top:8px"><div class="ico">i</div><div><b>${L("Si vous arrêtez", "If you stop")}</b><p>${L("Le clonage s'arrête ici. La destination ne reçoit pas de table de partitions et reste inutilisable ; la source n'a pas été modifiée.", "Cloning stops here. The destination receives no partition table and stays unusable; the source has not been modified.")}</p></div></div>
    <div class="actions"><button class="btn" data-act="unreadable-stop">${L("Arrêter le clonage", "Stop cloning")}</button><button class="btn danger" data-act="unreadable-zero">${L("Poursuivre malgré les données perdues", "Continue despite the lost data")}</button></div>`);
}

// ---------------------------------------------------------------- View: result
function outcomeHtml(rep, error, opts) {
  opts = opts || {};
  if (!rep) {
    return `<div class="verdict failed"><div class="badge">${badge("failed")}</div><div><h1>${L("Le clonage n'a pas eu lieu", "The clone did not take place")}</h1><ul><li class="selectable">${esc(error || L("Erreur inconnue.", "Unknown error."))}</li></ul></div></div>
      ${S.runBlocked?.length ? `<div class="panel" style="margin-top:14px"><h2>${L("Pré-vérifications", "Pre-checks")}</h2>${findingsHtml(S.runBlocked)}</div>` : ""}
      <div class="footer-bar"><span class="spacer"></span><button class="btn primary" data-act="restart">${L("Revenir à la sélection", "Back to selection")}</button></div>`;
  }
  const o = rep.outcome, st = STATUS[o.status];
  const dc = o.data_check, c = o.copy;
  const dataTxt = dc.mode === "none" ? L("Non vérifiées (relecture désactivée ou clonage interrompu)", "Not verified (read-back disabled or cloning interrupted)") : !dc.completed ? L(`Vérification interrompue (${size(dc.checked_bytes)} sur ${size(dc.total_bytes)})`, `Verification interrupted (${size(dc.checked_bytes)} of ${size(dc.total_bytes)})`) : dc.mismatches.length ? L(`${dc.mismatches.length} différence(s) sur ${size(dc.checked_bytes)} relus`, `${dc.mismatches.length} difference(s) in ${size(dc.checked_bytes)} read back`) : L(`${size(dc.checked_bytes)} relus, aucune différence`, `${size(dc.checked_bytes)} read back, no differences`);
  const good = o.status === "success_verified" || o.status === "completed_with_warnings";
  const physical = rep.destination.kind === "physical";
  // Clone finished but with sectors replaced by zeros: usable, damaged.
  const zeroed = o.table_written && o.bad_ranges.some((b) => b.outcome === "zero_filled");
  const next = [];
  if (zeroed) { const z = num(o.bad_ranges.reduce((a, b) => a + b.sectors, 0)); next.push(L(`Le clone est complet à l'exception de ${z} secteur(s) remplacé(s) par des zéros (liste ci-dessous). Relancer le clonage ne les récupérera probablement pas : le disque source est endommagé, évitez de le solliciter davantage.`, `The clone is complete except for ${z} sector(s) replaced with zeros (list below). Cloning again will probably not recover them: the source disk is damaged, avoid stressing it further.`)); }
  if ((good || zeroed) && physical) {
    next.push(L("Éteignez l'ordinateur, puis débranchez le disque source (ou mettez-le de côté). Tant que les deux disques sont branchés ensemble, Windows en garde un hors ligne car ils portent les mêmes identifiants.", "Shut the computer down, then unplug the source disk (or set it aside). While both disks are connected together, Windows keeps one of them offline because they carry the same identifiers."));
    next.push(L("Branchez le clone à la place de l'ancien disque et démarrez. Si l'ordinateur ne trouve pas le système, choisissez le nouveau disque dans le menu de démarrage du BIOS/UEFI.", "Connect the clone in place of the old disk and start the computer. If it does not find the system, pick the new disk in the BIOS/UEFI boot menu."));
    if (o.shrink && o.shrink.done) next.push(L(`Au premier démarrage, Windows affiche « Analyse et réparation du lecteur » pendant quelques minutes : il contrôle la partition réduite (${size(o.shrink.summary.old_size)} → ${size(o.shrink.summary.new_size)}). C'est prévu ; laissez-le aller au bout.`, `On first boot, Windows shows "Scanning and repairing drive" for a few minutes: it checks the shrunk partition (${size(o.shrink.summary.old_size)} → ${size(o.shrink.summary.new_size)}). This is expected; let it finish.`));
    if (o.grow && o.grow.done) next.push(L(`Au premier démarrage, Windows affiche « Analyse et réparation du lecteur » pendant quelques minutes : il contrôle la partition agrandie (${size(o.grow.summary.old_size)} → ${size(o.grow.summary.new_size)}). C'est prévu ; laissez-le aller au bout.`, `On first boot, Windows shows "Scanning and repairing drive" for a few minutes: it checks the grown partition (${size(o.grow.summary.old_size)} → ${size(o.grow.summary.new_size)}). This is expected; let it finish.`));
    if (rep.plan.free_after_main > 16 * 1048576) next.push(L(`Dans Windows, ouvrez « Gestion des disques », cliquez avec le bouton droit sur la partition principale puis « Étendre le volume » pour récupérer les ${size(rep.plan.free_after_main)} d'espace supplémentaire.`, `In Windows, open "Disk Management", right-click the main partition and choose "Extend Volume" to reclaim the ${size(rep.plan.free_after_main)} of extra space.`));
    next.push(L("Conservez l'ancien disque intact tant que vous n'avez pas vérifié que tout fonctionne sur le clone.", "Keep the old disk intact until you have checked that everything works on the clone."));
  }
  if (zeroed) {
    next.push(L("Une fois le clone en service, lancez « chkdsk X: /f » (X = lettre du volume, C: pour Windows) dans une invite de commandes administrateur, pour que Windows remette en ordre les structures éventuellement touchées.", "Once the clone is in use, run \"chkdsk X: /f\" (X = volume letter, C: for Windows) in an administrator command prompt, so that Windows repairs any affected structures."));
    next.push(L("Si des fichiers importants se trouvaient sur ces secteurs, gardez l'ancien disque : un service de récupération de données pourra peut-être en extraire davantage.", "If important files were on these sectors, keep the old disk: a data recovery service may be able to extract more."));
  } else if (o.status === "incomplete" || o.status === "failed") next.push(L("La source n'a pas été modifiée. Corrigez la cause indiquée ci-dessus (câble, disque de destination, espace…), puis relancez un clonage complet.", "The source has not been modified. Fix the cause shown above (cable, destination disk, space…), then run a complete clone again."));
  return `<div class="verdict ${st[0]}"><div class="badge">${badge(st[0])}</div><div><h1>${st[1]}</h1><ul class="selectable">${o.reasons.map((r) => `<li>${esc(r)}</li>`).join("")}</ul>
      <p class="dim" style="margin-top:8px">${esc(rep.source.model)} → ${esc(rep.destination.model)} · ${esc(rep.started_at)} · ${L("durée", "duration")} ${dur(o.duration_ms / 1000)}</p></div></div>
    <div class="stats" style="margin-top:14px">
      <div class="stat"><div class="k">${L("Données copiées", "Data copied")}</div><div class="v">${size(c.bytes_copied)}<small>/ ${size(c.bytes_planned)}</small></div></div>
      <div class="stat"><div class="k">${L("Débit moyen", "Average speed")}</div><div class="v">${c.avg_bps ? size(c.avg_bps) : "—"}<small>/s</small></div></div>
      <div class="stat ${o.bad_ranges.length ? "bad" : c.read_error_events ? "alert" : ""}"><div class="k">${L("Secteurs illisibles", "Unreadable sectors")}</div><div class="v">${num(o.bad_ranges.reduce((a, b) => a + b.sectors, 0))}</div></div>
      <div class="stat ${c.write_error_events ? "bad" : ""}"><div class="k">${L("Erreurs d'écriture", "Write errors")}</div><div class="v">${num(c.write_error_events)}</div></div>
    </div>
    ${next.length ? `<div class="panel" style="margin-top:14px"><h2>${L("Et maintenant ?", "What next?")}</h2><ol class="steps-next">${next.map((n) => `<li><span>${esc(n)}</span></li>`).join("")}</ol></div>` : ""}
    <div class="cols" style="margin-top:14px">
      <div class="panel" style="margin:0"><h2>${L("Vérifications effectuées", "Checks performed")}</h2><div class="checks">
        <div class="check ${o.table_check.performed ? (o.table_check.ok ? "y" : "n") : "u"}"><span class="s">${o.table_check.performed ? (o.table_check.ok ? "✓" : "×") : "?"}</span><span>${L("Table de partitions", "Partition table")} ${o.table_check.performed ? (o.table_check.ok ? L("conforme", "conforming") : L("non conforme : ", "not conforming: ") + esc(o.table_check.problems.join(" ; "))) : L("non contrôlée", "not checked")}</span></div>
        <div class="check ${dc.mode !== "none" && dc.completed && !dc.mismatches.length ? "y" : dc.mismatches.length ? "n" : "u"}"><span class="s">${dc.mode !== "none" && dc.completed && !dc.mismatches.length ? "✓" : dc.mismatches.length ? "×" : "?"}</span><span>${L("Données : ", "Data: ")}${esc(dataTxt)}</span></div>
        <div class="check ${!o.table_check.performed ? "u" : o.structure_findings.length ? "n" : "y"}"><span class="s">${!o.table_check.performed ? "?" : o.structure_findings.length ? "×" : "✓"}</span><span>${L("Systèmes de fichiers du clone", "Clone file systems")} ${!o.table_check.performed ? L("non contrôlés", "not checked") : o.structure_findings.length ? L("différents de la source", "differ from the source") : L("identiques à la source", "identical to the source")}</span></div>
        ${o.shrink ? `<div class="check ${o.shrink.done ? "y" : "n"}"><span class="s">${o.shrink.done ? "✓" : "×"}</span><span>${o.shrink.done ? L(`Partition ${o.shrink.number} réduite (${size(o.shrink.summary.old_size)} → ${size(o.shrink.summary.new_size)}, ${size(o.shrink.summary.relocated_clusters * o.shrink.summary.cluster_size)} déplacés), structures relues et contrôlées`, `Partition ${o.shrink.number} shrunk (${size(o.shrink.summary.old_size)} → ${size(o.shrink.summary.new_size)}, ${size(o.shrink.summary.relocated_clusters * o.shrink.summary.cluster_size)} moved), structures read back and checked`) : L(`Réduction de la partition ${o.shrink.number} non effectuée`, `Partition ${o.shrink.number} not shrunk`) + (o.shrink.error ? " : " + esc(o.shrink.error) : "")}</span></div>` : ""}
        ${o.grow ? `<div class="check ${o.grow.done ? "y" : o.grow.modified ? "n" : "u"}"><span class="s">${o.grow.done ? "✓" : o.grow.modified ? "×" : "?"}</span><span>${o.grow.done ? L(`Partition ${o.grow.number} agrandie (${size(o.grow.summary.old_size)} → ${size(o.grow.summary.new_size)}), structures relues et contrôlées`, `Partition ${o.grow.number} grown (${size(o.grow.summary.old_size)} → ${size(o.grow.summary.new_size)}), structures read back and checked`) : L(`Agrandissement de la partition ${o.grow.number} non effectué`, `Partition ${o.grow.number} not grown`) + (o.grow.error ? " : " + esc(o.grow.error) : "")}</span></div>` : ""}
        <div class="check ${!o.boot_destination ? "u" : o.boot_findings.length ? "n" : "y"}"><span class="s">${!o.boot_destination ? "?" : o.boot_findings.length ? "×" : "✓"}</span><span>${L("Éléments de démarrage", "Boot elements")} ${!o.boot_destination ? L("non contrôlés", "not checked") : o.boot_findings.length ? L("différents de la source", "differ from the source") : L("retrouvés sur le clone", "found on the clone")}</span></div>
      </div>${o.structure_findings.length || o.boot_findings.length ? `<div style="margin-top:10px">${findingsHtml([...o.structure_findings, ...o.boot_findings])}</div>` : ""}</div>
      <div class="panel" style="margin:0"><h2>${L("Démarrage du clone", "Clone boot")}</h2>${o.boot_destination?.checks.length ? `<div class="checks">${o.boot_destination.checks.map(checkRow).join("")}</div><p class="dim" style="margin-top:10px">${L("La présence de ces éléments ne garantit pas le démarrage, qui dépend aussi du micrologiciel et des réglages de la machine.", "The presence of these elements does not guarantee booting, which also depends on the machine's firmware and settings.")}</p>` : `<p class="muted">${o.boot_destination ? L("Aucun élément de démarrage : disque de données.", "No boot elements: data disk.") : L("Non contrôlé.", "Not checked.")}</p>`}</div>
    </div>
    ${o.bad_ranges.length ? `<div class="panel"><h2>${L("Secteurs illisibles de la source", "Unreadable source sectors")}</h2><table class="t selectable"><tr><th>Position (LBA)</th><th>${L("Secteurs", "Sectors")}</th><th>${L("Tentatives", "Attempts")}</th><th>${L("Erreur", "Error")}</th><th>${L("Traitement", "Handling")}</th></tr>${o.bad_ranges.map((b) => `<tr><td class="mono">${num(b.lba)} – ${num(b.lba + b.sectors - 1)}</td><td class="num">${num(b.sectors)}</td><td class="num">${b.attempts}</td><td>${esc(b.error)}</td><td>${{ zero_filled: L("Remplacé par des zéros", "Replaced with zeros"), not_copied: L("Non copié", "Not copied"), pending: L("Non traité", "Not handled") }[b.outcome]}</td></tr>`).join("")}</table></div>` : ""}
    ${o.recovered.length ? `<div class="panel"><h2>${L("Secteurs lus après nouvelle tentative", "Sectors read after a retry")}</h2><table class="t selectable"><tr><th>Position (LBA)</th><th>${L("Secteurs", "Sectors")}</th><th>${L("Tentative réussie", "Successful attempt")}</th></tr>${o.recovered.slice(0, 200).map((x) => `<tr><td class="mono">${num(x.lba)} – ${num(x.lba + x.sectors - 1)}</td><td class="num">${num(x.sectors)}</td><td class="num">${x.attempt}</td></tr>`).join("")}</table></div>` : ""}
    ${dc.mismatches.length ? `<div class="panel"><h2>${L("Écarts relevés à la vérification", "Differences found during verification")}</h2><table class="t selectable"><tr><th>LBA</th><th>${L("Secteurs", "Sectors")}</th><th>${L("Écart", "Difference")}</th></tr>${dc.mismatches.slice(0, 200).map((m) => `<tr><td class="mono">${num(m.lba)}</td><td class="num">${num(m.sectors)}</td><td>${esc(m.detail)}</td></tr>`).join("")}</table></div>` : ""}
    <div class="cols" style="margin-top:14px"><div class="panel" style="margin:0"><h2>Source</h2>${identity(rep.source)}</div><div class="panel" style="margin:0"><h2>Destination</h2>${identity(rep.destination, true)}</div></div>
    <div class="panel"><h2>${L("Pré-vérifications", "Pre-checks")}</h2>${findingsHtml(rep.precheck)}</div>
    <details class="log"><summary>${L(`Journal complet (${rep.log.length} lignes)`, `Full log (${rep.log.length} lines)`)}</summary><pre>${rep.log.map((e) => `<div><span class="t">${esc(e.time)}</span> <span class="${e.level === "warning" ? "w" : e.level === "error" ? "e" : ""}">${esc(e.message)}</span></div>`).join("")}</pre></details>
    <div class="footer-bar"><span class="muted">${L(`Rapport ${esc(rep.id)} enregistré.`, `Report ${esc(rep.id)} saved.`)}</span><span class="spacer"></span>
      <button class="btn small" data-act="export" data-id="${esc(rep.id)}" data-fmt="txt">${L("Exporter en TXT", "Export as TXT")}</button><button class="btn small" data-act="export" data-id="${esc(rep.id)}" data-fmt="html">${L("Exporter en HTML", "Export as HTML")}</button>
      ${opts.fromList ? `<button class="btn" data-act="report-close">← ${L("Tous les rapports", "All reports")}</button>` : `<button class="btn primary" data-act="restart">${L("Nouveau clonage", "New clone")}</button>`}</div>`;
}

const itemGood = (it) => !!it.report && ["success_verified", "completed_with_warnings"].includes(it.report.outcome.status);

function queueSummary() {
  const q = S.queue;
  if (!q || !q.items.length) return "";
  const ej = (it) => it.ejected === true ? `<span class="chip ok">${L("éjecté", "ejected")}</span>` : it.ejected === false ? `<span class="chip warn" title="${esc(it.ejectError || "")}">${L("non éjecté", "not ejected")}</span>` : "";
  if (q.items.length === 1) {
    const it = q.items[0];
    return it.ejected != null ? `<div class="panel slim">${ej(it)} ${it.ejectError ? `<span class="muted">${esc(it.ejectError)}</span>` : ""}</div>` : "";
  }
  return `<div class="panel"><h2>${L("File de clonage", "Clone queue")}</h2><table class="t"><tr><th>#</th><th>Destination</th><th>${L("Résultat", "Result")}</th><th>${L("Durée", "Duration")}</th><th></th><th></th></tr>${q.items.map((it, i) => {
    const st = it.report ? STATUS[it.report.outcome.status] : ["failed", it.blocked?.length ? L("Refusé", "Refused") : L("Erreur", "Error")];
    return `<tr class="${i === S.resultIdx ? "sel" : ""}"><td>${i + 1}</td><td>${esc(it.prep.destination.model)} <span class="dim mono">${size(it.prep.destination.size)}</span></td><td><span class="chip ${{ ok: "ok", warn: "warn", incomplete: "bad", failed: "bad" }[st[0]]}">${st[1]}</span></td><td class="num">${it.report ? dur(it.report.outcome.duration_ms / 1000) : "—"}</td><td>${ej(it)}</td><td><button class="btn small" data-act="result-sel" data-i="${i}" ${i === S.resultIdx ? "disabled" : ""}>${L("Détails", "Details")}</button></td></tr>`;
  }).join("")}</table></div>`;
}

function badge(kind) {
  const col = { ok: "#54f0b6", warn: "#ffc56b", incomplete: "#ff8a5c", failed: "#ff4f82" }[kind];
  const twin = kind === "ok" || kind === "warn";
  const mark = kind === "ok" ? `<path d="M36 51l9 9 19-21" fill="none" stroke="#04101c" stroke-width="6" stroke-linecap="round" stroke-linejoin="round"/>` : kind === "warn" ? `<path d="M50 34v20M50 64v2" stroke="#1c1204" stroke-width="6.5" stroke-linecap="round"/>` : kind === "incomplete" ? `<path d="M36 50h28" stroke="#1c0a04" stroke-width="6.5" stroke-linecap="round"/>` : `<path d="M38 38l24 24M62 38L38 62" stroke="#1c0410" stroke-width="6.5" stroke-linecap="round"/>`;
  return `<svg viewBox="0 0 100 100"><circle cx="50" cy="50" r="46" fill="${col}" opacity=".12"/><circle class="${twin ? "" : "twinkle"}" cx="50" cy="50" r="46" fill="none" stroke="${col}" stroke-opacity=".5" stroke-dasharray="${twin ? "none" : "4 7"}"/><circle cx="50" cy="50" r="30" fill="${col}"/>${mark}</svg>`;
}

// ------------------------------------------------------- View: disks & health
// Boot repair of a Windows disk for another PC (BCD, storage drivers, hibernation).
// Always shown as its own panel, so the feature can be found before any analysis.
function fixBootHtml(d, ins) {
  const intro = `<p class="muted">${L("Pour un disque cloné qui affiche « Votre PC doit être réparé » (0xc000000e) ou un écran bleu au démarrage sur son nouveau PC.", "For a cloned disk that shows \"Your PC needs to be repaired\" (0xc000000e) or a blue screen at boot on its new PC.")}</p>`;
  const note = (t) => `${intro}<p class="dim" style="margin-top:8px">${t}</p>`;
  if (d.kind !== "physical") return note(L("Disponible uniquement pour un disque physique.", "Only available for a physical disk."));
  if (d.is_system) return note(L("C'est le disque sur lequel ce Windows tourne : il ne peut pas être réparé d'ici. Branche le disque à réparer sur ce PC, ou démarre sur la clé Chud Clone (menu R).", "This is the disk this Windows runs from: it cannot be repaired from here. Connect the disk to repair to this PC, or boot the Chud Clone key (menu R)."));
  if (S.busy["inspect:" + d.id]) return `${intro}<div class="loader"></div>`;
  if (!ins || ins.error) return `${note(L("Le disque doit d'abord être analysé pour vérifier qu'il contient Windows (lecture seule).", "The disk must first be analysed to check that it contains Windows (read-only)."))}<button class="btn small" style="margin-top:10px" data-act="inspect" data-id="${esc(d.id)}">${L("Analyser ce disque", "Analyse this disk")}</button>`;
  if (!["windows_uefi", "windows_bios"].includes(ins.boot.style)) return note(L("Aucun Windows trouvé sur ce disque : rien à réparer.", "No Windows found on this disk: nothing to repair."));
  const f = S.fix[d.id] || {};
  const steps = f.steps?.length ? `<ol class="fmt-steps">${f.steps.map((x) => `<li>${esc(x)}</li>`).join("")}</ol>` : "";
  let body;
  if (f.busy) body = `<div class="loader"></div>${steps}`;
  else if (f.error) body = `<div class="finding error"><div class="ico">×</div><div><b>${L("Réparation impossible", "Repair failed")}</b><p class="selectable">${esc(f.error)}</p></div></div>${steps}`;
  else if (f.result) {
    const r = f.result, ok = !r.warnings.length;
    body = `<div class="finding ${ok ? "info" : "warning"}"><div class="ico">${ok ? "✓" : "!"}</div><div><b>${ok ? L("Démarrage réparé", "Boot repaired") : L("Réparation partielle", "Partial repair")}</b>
      <ul>${r.bcd_fixed ? `<li class="okc">${L("Configuration de démarrage (BCD) : désigne les partitions de ce disque", "Boot configuration (BCD): designates this disk's partitions")}</li>` : ""}${r.drivers.length ? `<li class="okc">${L("Pilotes activés au démarrage : ", "Drivers enabled at boot: ")}<span class="val">${esc(r.drivers.join(", "))}</span></li>` : ""}${r.hibernation_cleared ? `<li class="okc">${L("Session en veille prolongée annulée", "Hibernated session cancelled")}</li>` : ""}${r.fallback_added ? `<li class="okc">${L("Chargeur UEFI de secours ajouté", "Fallback UEFI loader added")}</li>` : ""}${(r.reserved_cleared || []).length ? `<li class="okc">${L("Ancienne partition de démarrage effacée de la partition réservée", "Former boot partition erased from the reserved partition")}</li>` : ""}${r.warnings.map((w) => `<li class="warnc">${esc(w)}</li>`).join("")}</ul></div></div>`;
  } else body = "";
  const needAdmin = S.env && !S.env.elevated;
  return `<div>
    <p class="muted">${L("Pour un disque qui affiche « Votre PC doit être réparé » (0xc000000e) ou un écran bleu au démarrage sur son nouveau PC : la configuration de démarrage est réécrite pour désigner les partitions de ce disque, les pilotes NVMe et AHCI démarrent avec Windows, une session en veille prolongée est annulée et le chargeur UEFI de secours est ajouté s'il manque. Aucune donnée n'est effacée.", "For a disk that shows \"Your PC needs to be repaired\" (0xc000000e) or a blue screen at boot on its new PC: the boot configuration is rewritten to designate this disk's partitions, the NVMe and AHCI drivers start with Windows, a hibernated session is cancelled and the fallback UEFI loader is added if missing. No data is erased.")}</p>
    ${body}
    <div class="row" style="margin-top:10px">${needAdmin ? `<button class="btn small primary" data-act="elevate">${L("Passer en administrateur", "Switch to administrator")}</button>` : `<button class="btn small" data-act="fixboot" data-id="${esc(d.id)}" ${f.busy || activity() !== "idle" ? "disabled" : ""}>${f.result ? L("Réparer à nouveau", "Repair again") : L("Réparer le démarrage", "Repair boot")}</button>`}</div></div>`;
}

function viewDisks() {
  if (S.loadingDisks) return `<div class="page"><div class="empty"><div class="orbit-loader"></div>${L("Recherche des disques…", "Looking for disks…")}</div></div>`;
  const sel = disk(S.diskSel) || S.disks[0];
  if (!sel) return `<div class="page"><div class="empty">${L("Aucun disque détecté.", "No disk detected.")}</div></div>`;
  const ins = S.inspect[sel.id], sm = S.smart[sel.id];
  const parts = sel.partitions.length
    ? `<table class="t selectable"><tr><th>${L("N°", "No.")}</th><th>Type</th><th>${L("Système", "File system")}</th><th>${L("Nom", "Name")}</th><th>${L("Lecteur", "Drive")}</th><th>${L("Taille", "Size")}</th><th>${L("Utilisé", "Used")}</th></tr>${sel.partitions.map((p) => `<tr><td>${p.number}</td><td>${esc(p.type_label)}${p.active ? ` <span class="chip">active</span>` : ""}</td><td>${esc(p.fs || "—")}</td><td>${esc(p.label || p.name || "—")}</td><td>${esc(p.mounts.join(" ") || "—")}</td><td class="num">${size(p.size)}</td><td class="num">${p.used_bytes != null ? size(p.used_bytes) : "—"}</td></tr>`).join("")}</table>`
    : `<p class="muted">${L("Aucune partition.", "No partition.")}</p>`;
  let smartHtml;
  if (S.busy["smart:" + sel.id]) smartHtml = `<div class="loader"></div>`;
  else if (!sm) smartHtml = `<p class="muted">${L("L'état de santé est lu directement sur le disque (commande en lecture seule).", "Health is read directly from the disk (read-only command).")}</p><button class="btn small" style="margin-top:10px" data-act="smart" data-id="${esc(sel.id)}">${L("Lire l'état de santé", "Read health")}</button>`;
  else if (!sm.available) smartHtml = `<div class="gauge"><span class="lv unknown"></span><div><b>${L("Non disponible", "Not available")}</b><div class="muted">${esc(sm.unavailable_reason || "")}</div></div></div>`;
  else {
    smartHtml = `<div class="gauge"><span class="lv ${sm.level}"></span><div><b style="font-size:15px">${HEALTH[sm.level]}</b><div class="muted">${L(`Indicateur fondé sur les compteurs du disque (${esc(sm.protocol)}). Une estimation, pas une certitude sur son état physique.`, `Indicator based on the disk's counters (${esc(sm.protocol)}). An estimate, not a certainty about its physical condition.`)}</div></div></div>
      <div class="metrics">
        <div class="stat"><div class="k">${L("Température", "Temperature")}</div><div class="v">${sm.temperature_c != null ? sm.temperature_c + "<small>°C</small>" : "—"}</div></div>
        <div class="stat"><div class="k">${L("Fonctionnement", "Power-on time")}</div><div class="v">${sm.power_on_hours != null ? num(sm.power_on_hours) + "<small>h</small>" : "—"}</div></div>
        <div class="stat"><div class="k">${L("Mises sous tension", "Power cycles")}</div><div class="v">${sm.power_cycles != null ? num(sm.power_cycles) : "—"}</div></div>
        <div class="stat"><div class="k">${L("Usure estimée", "Estimated wear")}</div><div class="v">${sm.wear_percent_used != null ? sm.wear_percent_used + "<small>%</small>" : "—"}</div></div>
      </div>
      ${sm.findings.length ? `<div style="margin-top:12px">${findingsHtml(sm.findings)}</div>` : ""}
      ${healthHistoryHtml(S.health[sel.id], sel)}
      ${sm.nvme ? `<div style="margin-top:12px">${kv([[L("Réserve disponible", "Available spare"), `${sm.nvme.available_spare} % (${L("seuil", "threshold")} ${sm.nvme.spare_threshold} %)`], [L("Erreurs de support", "Media errors"), num(sm.nvme.media_errors)], [L("Arrêts brutaux", "Unsafe shutdowns"), num(sm.nvme.unsafe_shutdowns)], [L("Total écrit", "Total written"), size(sm.nvme.data_written_bytes)], [L("Total lu", "Total read"), size(sm.nvme.data_read_bytes)]])}</div>` : ""}
      ${sm.attributes.length ? `<details class="adv" style="margin-top:12px"><summary>${L("Attributs SMART détaillés", "Detailed SMART attributes")} (${sm.attributes.length})</summary><div class="body"><table class="t selectable"><tr><th>ID</th><th>${L("Attribut", "Attribute")}</th><th>${L("Valeur", "Value")}</th><th>${L("Pire", "Worst")}</th><th>${L("Seuil", "Threshold")}</th><th>${L("Brut", "Raw")}</th></tr>${sm.attributes.map((a) => `<tr${a.below_threshold ? ` style="color:var(--danger)"` : ""}><td>${a.id}</td><td>${esc(a.name)}</td><td class="num">${a.current}</td><td class="num">${a.worst}</td><td class="num">${a.threshold ?? "—"}</td><td class="num mono">${num(a.raw)}</td></tr>`).join("")}</table></div></details>` : ""}`;
  }
  let insHtml;
  if (S.busy["inspect:" + sel.id]) insHtml = `<div class="loader"></div>`;
  else if (!ins) insHtml = `<p class="muted">${L("Lit la table de partitions et les systèmes de fichiers pour y chercher des incohérences. Lecture seule.", "Reads the partition table and file systems to look for inconsistencies. Read-only.")}</p><button class="btn small" style="margin-top:10px" data-act="inspect" data-id="${esc(sel.id)}">${L("Analyser ce disque", "Analyse this disk")}</button>`;
  else if (ins.error) insHtml = `<div class="finding error"><div class="ico">×</div><div><b>${L("Analyse impossible", "Analysis failed")}</b><p class="selectable">${esc(ins.error)}</p></div></div>`;
  else insHtml = `<div class="row wrap" style="gap:22px;margin-bottom:12px"><div><div class="dim">${L("Type de disque", "Disk type")}</div><b>${BOOT[ins.boot.style]}</b></div></div>
      ${ins.boot.checks.length ? `<div class="checks" style="margin-bottom:12px">${ins.boot.checks.map(checkRow).join("")}</div>` : ""}${findingsHtml(ins.findings)}`;
  return `<div class="page"><div class="page-head"><div><h1>${L("Disques et santé", "Disks & health")}</h1><p>${L("Identité, partitions et indicateurs de santé. Tout ici est en lecture seule, sauf la réparation du démarrage.", "Identity, partitions and health indicators. Everything here is read-only, except the boot repair.")}</p></div><button class="btn small ghost" data-act="refresh">↻ ${L("Actualiser", "Refresh")}</button></div>
    <div class="split"><div class="disk-list">${S.disks.map((d) => diskCard(d, { mini: true, act: "disk-sel", selected: d.id === sel.id })).join("")}</div>
    <div>
      <div class="panel"><div class="row" style="gap:16px;margin-bottom:14px"><div style="width:74px;height:74px;flex:none">${Cosmos.planet(sel)}</div><div style="min-width:0"><h3 style="font-size:18px;font-weight:650">${esc(sel.model || L("Disque sans nom", "Unnamed disk"))}</h3><div class="chips">${diskChips(sel)}</div></div></div>${identity(sel)}
        ${sel.notes.length ? `<div style="margin-top:10px">${sel.notes.map((n) => `<div class="dim">· ${esc(n)}</div>`).join("")}</div>` : ""}</div>
      <div class="panel"><h2>Partitions</h2>${strip(sel.partitions, sel.size, false)}<div style="margin-top:12px">${parts}</div></div>
      <div class="panel"><h2>${L("État de santé", "Health")}</h2>${smartHtml}</div>
      <div class="panel"><h2>${L("Diagnostic des partitions et du démarrage", "Partition and boot diagnostics")}</h2>${insHtml}</div>
      <div class="panel fixboot" id="fixboot-panel"><h2>${L("Réparer le démarrage pour un autre PC", "Repair boot for another PC")}</h2>${fixBootHtml(sel, ins)}</div>
      <div class="panel" id="scan-panel" data-id="${esc(sel.id)}"><h2>${L("Scan de surface", "Surface scan")}</h2>${scanHtml(sel)}</div>
    </div></div></div>`;
}

// ---------------------------------------------------------- Scan de surface
const SCAN_COLORS = ["rgba(150,180,255,.55)", "#4fe3ff", "#54f0b6", "#ffc56b", "#ff8a5c", "#ff4f82", "#ff2d55"];

function scanEstimate(d) {
  const bps = d.bus === "USB" ? 35e6 : d.bus === "NVMe" ? 1.5e9 : d.media === "ssd" ? 450e6 : d.media === "hdd" ? 130e6 : 200e6;
  return dur(d.size / bps);
}

function scanHtml(d) {
  const sc = S.scan;
  const mine = sc && sc.id === d.id;
  if (sc?.running && !mine) return `<p class="muted">${L("Un scan est en cours sur un autre disque. Un seul scan à la fois.", "A scan is running on another disk. One scan at a time.")}</p>`;
  const intro = `<p class="muted">${L("Relit toute la surface du disque et mesure le temps de réponse de chaque zone. Les zones lentes annoncent souvent des secteurs qui vont devenir illisibles. <b>Lecture seule</b> : rien n'est écrit, aucune donnée n'est modifiée.", "Reads the whole disk surface and measures the response time of each area. Slow areas often herald sectors that will become unreadable. <b>Read-only</b>: nothing is written, no data is changed.")}</p>`;
  if (!mine) {
    const needAdmin = d.kind === "physical" && S.env && !S.env.elevated;
    return `${intro}<div class="row wrap" style="margin-top:12px;gap:14px">
      ${needAdmin ? `<button class="btn small primary" data-act="elevate">${L("Passer en administrateur pour scanner", "Switch to administrator to scan")}</button>` : `<button class="btn small" data-act="scan" data-id="${esc(d.id)}" ${S.run ? "disabled" : ""}>${L("Lancer le scan", "Start the scan")}</button>`}
      <span class="dim">${L(`Durée estimée : environ ${scanEstimate(d)} pour ${size(d.size)}. Le disque reste utilisable pendant le scan, mais plus lent.`, `Estimated duration: about ${scanEstimate(d)} for ${size(d.size)}. The disk stays usable during the scan, but slower.`)}</span></div>`;
  }
  const p = sc.progress, r = sc.result;
  const counts = r ? r.counts : p ? p.counts : [0, 0, 0, 0, 0, 0, 0];
  const legend = `<div class="legend scan-legend">${SCAN_LABELS.map((l, i) => `<span style="--c:${SCAN_COLORS[i]}">${l} <b>${num(counts[i])}</b></span>`).join("")}</div>`;
  let head;
  if (sc.error) head = `<div class="finding error"><div class="ico">×</div><div><b>${L("Scan impossible", "Scan failed")}</b><p class="selectable">${esc(sc.error)}</p></div></div>`;
  else if (sc.running) {
    const pct = p && p.total ? (p.done / p.total) * 100 : 0;
    head = `<div class="row wrap" style="gap:18px;align-items:center">
      <div class="scan-pct">${dec(pct.toFixed(1))}<small>%</small></div>
      <div><div class="dim">${L("Débit", "Speed")}</div><b>${p ? size(p.speed_bps) + "/s" : "—"}</b></div>
      <div><div class="dim">${L("Lu", "Read")}</div><b>${p ? size(p.done) : "—"}</b> <span class="dim">/ ${size(d.size)}</span></div>
      <div><div class="dim">${L("Temps restant", "Time left")}</div><b>${p?.eta_secs != null ? "≈ " + dur(p.eta_secs) : L("calcul…", "estimating…")}</b></div>
      <div><div class="dim">${L("Secteurs illisibles", "Unreadable sectors")}</div><b style="color:${p?.bad_sectors ? "var(--danger)" : "inherit"}">${num(p?.bad_sectors)}</b></div>
      <span class="spacer"></span><button class="btn small" data-act="scan-cancel" ${sc.cancelling ? "disabled" : ""}>${sc.cancelling ? L("Arrêt…", "Stopping…") : L("Arrêter le scan", "Stop the scan")}</button></div>`;
  } else head = `<div class="row wrap" style="gap:18px;margin-bottom:4px"><div><div class="dim">${L("Lu", "Read")}</div><b>${size(r.scanned_bytes)}</b> <span class="dim">/ ${size(r.size)}</span></div><div><div class="dim">${L("Durée", "Duration")}</div><b>${dur(r.duration_ms / 1000)}</b></div><div><div class="dim">${L("Débit moyen", "Average speed")}</div><b>${size(r.avg_bps)}/s</b></div><span class="spacer"></span><button class="btn small ghost" data-act="scan" data-id="${esc(d.id)}">↻ ${L("Relancer", "Run again")}</button></div>${findingsHtml(r.findings)}`;
  const bad = r && r.bad.length ? `<details class="adv" style="margin-top:12px"><summary>${L("Secteurs illisibles", "Unreadable sectors")} (${num(r.bad_sectors)})</summary><div class="body"><table class="t selectable"><tr><th>Position (LBA)</th><th>${L("Secteurs", "Sectors")}</th><th>${L("Erreur", "Error")}</th></tr>${r.bad.slice(0, 300).map((b) => `<tr><td class="mono">${num(b.lba)}${b.sectors > 1 ? " – " + num(b.lba + b.sectors - 1) : ""}</td><td class="num">${num(b.sectors)}</td><td>${esc(b.error)}</td></tr>`).join("")}</table></div></details>` : "";
  const slow = r && r.slowest.length ? `<details class="adv" style="margin-top:8px"><summary>${L("Zones les plus lentes", "Slowest areas")} (${r.slowest.length})</summary><div class="body"><table class="t selectable"><tr><th>Position (LBA)</th><th>${L("Temps de lecture", "Read time")}</th></tr>${r.slowest.map((s) => `<tr><td class="mono">${num(s.lba)}</td><td class="num">${s.ms === 4294967295 ? L("relue après une erreur", "read after an error") : num(s.ms) + " ms"}</td></tr>`).join("")}</table></div></details>` : "";
  return `${head}<div class="scan-map"><canvas id="scanmap"></canvas></div>${legend}
    <p class="dim" style="margin-top:6px">${L("Chaque étoile représente une zone du disque, du début (en haut à gauche) à la fin. Plus elle est chaude et grosse, plus la lecture a été difficile.", "Each star is an area of the disk, from the start (top left) to the end. The hotter and bigger it is, the harder the read was.")}</p>${bad}${slow}${r && sc.saved ? `<p class="dim" style="margin-top:8px">${L("Résultat enregistré : ", "Result saved: ")}<code>${esc(sc.saved)}</code></p>` : ""}`;
}

// Updates only the scan panel (no full render every 200 ms).
function refreshScanPanel() {
  const panel = $("#scan-panel");
  if (!panel || S.view !== "disks" || panel.dataset.id !== S.scan?.id) return;
  const d = disk(S.scan.id);
  if (!d) return;
  panel.innerHTML = `<h2>${L("Scan de surface", "Surface scan")}</h2>${scanHtml(d)}`;
  drawScanMap();
}

function drawScanMap() {
  const cv = $("#scanmap"), sc = S.scan;
  if (!cv || !sc) return;
  const w = cv.parentElement.clientWidth, n = sc.cells.length;
  const cols = Math.max(1, Math.ceil(Math.sqrt(n * (w / 220))));
  const rows = Math.ceil(n / cols), gap = w / cols, h = Math.max(60, Math.ceil(rows * gap));
  const dpr = window.devicePixelRatio || 1;
  if (cv.width !== Math.round(w * dpr) || cv.height !== Math.round(h * dpr)) {
    cv.width = Math.round(w * dpr); cv.height = Math.round(h * dpr);
    cv.style.width = w + "px"; cv.style.height = h + "px";
  }
  const g = cv.getContext("2d");
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  g.clearRect(0, 0, w, h);
  for (let i = 0; i < n; i++) {
    const c = sc.cells[i];
    // Slight stable pseudo-random offset: a star field rather than a grid.
    const jx = ((i * 2654435761) % 1000) / 1000 - 0.5, jy = ((i * 40503) % 1000) / 1000 - 0.5;
    const x = (i % cols) * gap + gap / 2 + jx * gap * 0.35, y = Math.floor(i / cols) * gap + gap / 2 + jy * gap * 0.35;
    if (c === 255) { g.fillStyle = "rgba(255,255,255,.07)"; g.fillRect(x - 0.6, y - 0.6, 1.2, 1.2); continue; }
    const r = Math.min(gap * 0.48, [0.9, 1.1, 1.4, 1.9, 2.5, 3.1, 3.3][c] * Math.max(1, gap / 7));
    if (c >= 3) {
      const grd = g.createRadialGradient(x, y, 0, x, y, r * 3);
      grd.addColorStop(0, SCAN_COLORS[c]); grd.addColorStop(1, "rgba(0,0,0,0)");
      g.fillStyle = grd; g.globalAlpha = 0.45; g.beginPath(); g.arc(x, y, r * 3, 0, 7); g.fill(); g.globalAlpha = 1;
    }
    g.fillStyle = SCAN_COLORS[c]; g.beginPath(); g.arc(x, y, r, 0, 7); g.fill();
    if (c === 6) { g.strokeStyle = "#fff"; g.lineWidth = 1; g.beginPath(); g.moveTo(x - r, y - r); g.lineTo(x + r, y + r); g.moveTo(x + r, y - r); g.lineTo(x - r, y + r); g.stroke(); }
  }
  if (sc.running && sc.cursor != null) {
    const i = sc.cursor, x = (i % cols) * gap + gap / 2, y = Math.floor(i / cols) * gap + gap / 2;
    g.strokeStyle = "rgba(79,227,255,.9)"; g.lineWidth = 1.2; g.beginPath(); g.arc(x, y, Math.max(4, gap * 0.7), 0, 7); g.stroke();
  }
}

// Health over time: temperature (curve) and defect counters (steps).
function healthHistoryHtml(h, d) {
  if (!d.serial) return `<p class="dim" style="margin-top:12px">${L("Sans numéro de série, ce disque ne peut pas être reconnu d'une fois sur l'autre : son historique n'est pas conservé.", "Without a serial number this disk cannot be recognised from one time to the next: its history is not kept.")}</p>`;
  if (!h) return "";
  const snaps = h.history.snapshots;
  const date = (t) => new Date(t * 1000).toLocaleDateString(LOCALE());
  const trends = h.trends.length ? `<div style="margin-top:12px"><div class="dim" style="margin-bottom:6px">${L("Évolution depuis le premier relevé", "Change since the first reading")}</div>${findingsHtml(h.trends)}</div>` : "";
  if (snaps.length < 2) return `${trends}<p class="dim" style="margin-top:12px">${L(`Premier relevé enregistré le ${date(snaps[0]?.at ?? Date.now() / 1000)}. Les prochains permettront de suivre l'évolution de ce disque et de signaler ce qui s'aggrave.`, `First reading saved on ${date(snaps[0]?.at ?? Date.now() / 1000)}. The next ones will track how this disk evolves and flag what gets worse.`)}</p>`;
  const W = 640, H = 150, P = 26;
  const t0 = snaps[0].at, t1 = snaps[snaps.length - 1].at || t0 + 1;
  const x = (t) => P + ((t - t0) / Math.max(1, t1 - t0)) * (W - 2 * P);
  const temps = snaps.filter((s) => s.temperature_c != null);
  const tMax = Math.max(60, ...temps.map((s) => s.temperature_c)), tMin = Math.min(20, ...temps.map((s) => s.temperature_c));
  const yT = (v) => H - P - ((v - tMin) / (tMax - tMin)) * (H - 2 * P);
  const series = [["reallocated", "#ffc56b", L("Réalloués", "Reallocated")], ["pending", "#ff4f82", L("En attente", "Pending")], ["offline_uncorrectable", "#ff8a5c", L("Non corrigibles", "Uncorrectable")], ["media_errors", "#ff4f82", L("Erreurs NVMe", "NVMe errors")], ["crc_errors", "#9a6bff", L("Erreurs CRC", "CRC errors")]]
    .filter(([k]) => snaps.some((s) => s[k] > 0));
  const cMax = Math.max(1, ...series.flatMap(([k]) => snaps.map((s) => s[k] || 0)));
  const yC = (v) => H - P - (v / cMax) * (H - 2 * P);
  const step = (k) => snaps.map((s, i) => `${i ? "H" : "M"}${x(s.at).toFixed(1)} ${i ? "" : yC(s[k] || 0).toFixed(1)}${i ? `V${yC(s[k] || 0).toFixed(1)}` : ""}`).join(" ");
  const tempPath = temps.map((s, i) => `${i ? "L" : "M"}${x(s.at).toFixed(1)} ${yT(s.temperature_c).toFixed(1)}`).join(" ");
  const legend = [`<span style="--c:#4fe3ff">${L("Température", "Temperature")} (${tMin}–${tMax} °C)</span>`, ...series.map(([k, c, l]) => `<span style="--c:${c}">${l} (max ${num(Math.max(...snaps.map((s) => s[k] || 0)))})</span>`)].join("");
  return `${trends}<div style="margin-top:14px"><div class="dim" style="margin-bottom:6px">${L(`${snaps.length} relevés, du ${date(t0)} au ${date(t1)}`, `${snaps.length} readings, from ${date(t0)} to ${date(t1)}`)}</div>
    <svg class="hchart" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img" aria-label="${L("Évolution de la santé du disque", "Disk health over time")}">
      <line x1="${P}" y1="${H - P}" x2="${W - P}" y2="${H - P}" stroke="rgba(255,255,255,.12)"/>
      ${series.map(([k, c]) => `<path d="${step(k)}" fill="none" stroke="${c}" stroke-width="2"/>`).join("")}
      ${tempPath ? `<path d="${tempPath}" fill="none" stroke="#4fe3ff" stroke-width="1.6" stroke-opacity=".85"/>` : ""}
      ${snaps.map((s) => `<circle cx="${x(s.at).toFixed(1)}" cy="${H - P}" r="2" fill="rgba(255,255,255,.35)"><title>${date(s.at)}${s.temperature_c != null ? " — " + s.temperature_c + " °C" : ""}</title></circle>`).join("")}
    </svg><div class="legend">${legend}</div></div>`;
}

// ------------------------------------------------------------ Vue : rapports
function viewReports() {
  if (S.reportOpen) return `<div class="page">${outcomeHtml(S.reportOpen, null, { fromList: true })}</div>`;
  let body;
  if (!S.reports) body = `<div class="empty"><div class="loader"></div></div>`;
  else if (!S.reports.length) body = `<div class="panel empty">${L("Aucun rapport pour l'instant. Chaque clonage en produit un, enregistré sur cet ordinateur.", "No reports yet. Every clone produces one, saved on this computer.")}</div>`;
  else body = `<div class="panel"><table class="t"><tr><th>Date</th><th>${L("Résultat", "Result")}</th><th>Source → destination</th><th>${L("Copié", "Copied")}</th><th>${L("Durée", "Duration")}</th><th></th></tr>${S.reports
    .map((r) => `<tr><td>${esc(r.started_at)}</td><td><span class="chip ${{ ok: "ok", warn: "warn", incomplete: "bad", failed: "bad" }[STATUS[r.status][0]]}">${STATUS[r.status][1]}</span></td><td>${esc(r.source)} → ${esc(r.destination)}</td><td class="num">${size(r.bytes_copied)}</td><td class="num">${dur(r.duration_ms / 1000)}</td><td><button class="btn small" data-act="report-open" data-id="${esc(r.id)}">${L("Ouvrir", "Open")}</button></td></tr>`)
    .join("")}</table></div>`;
  return `<div class="page"><div class="page-head"><div><h1>${L("Rapports", "Reports")}</h1><p>${L("Enregistrés localement dans", "Saved locally in")} <code>${esc(S.env?.reports_dir || "")}</code>. ${L("Rien n'est envoyé sur Internet.", "Nothing is sent over the Internet.")}</p></div></div>${body}</div>`;
}

// ---------------------------------------------------------------- View: format
const FS_MAX = { ntfs: 32, exfat: 15, fat32: 11 };
function viewFormat() {
  const f = S.fmt;
  if (S.loadingDisks) return `<div class="page"><div class="empty"><div class="orbit-loader"></div></div></div>`;
  const fm = M.format;
  const list = S.disks.map((d) => diskCard(d, { mini: true, act: "fmt-sel", selected: d.id === f.id, locked: fm.is("running") })).join("");
  const sel = disk(f.id);
  let right;
  if (!sel) right = `<div class="panel empty">${L("Choisir un disque à gauche.", "Pick a disk on the left.")}</div>`;
  else {
    const c = f.check;
    const word = confirmWord();
    const good = f.confirm.trim().toUpperCase() === word;
    const refused = c?.refusals || [];
    const notes = [];
    if (f.fs === "fat32") notes.push(L("FAT32 : fichiers de 4 Go maximum.", "FAT32: 4 GB maximum file size."));
    if (c?.own_fat32) notes.push(L("FAT32 > 32 Go : écrit par Chud Clone (Windows refuse au-delà).", "FAT32 > 32 GB: written by Chud Clone (Windows refuses beyond)."));
    if (f.table === "mbr") notes.push(L("MBR : démarrage BIOS hérité, 2 To max.", "MBR: legacy BIOS boot, 2 TB max."));
    const sel1 = (k, list) => `<select data-fmt="${k}" ${fm.is("running") ? "disabled" : ""}>${list.map(([v, l]) => `<option value="${v}" ${f[k] === v ? "selected" : ""}>${l}</option>`).join("")}</select>`;
    const form = `<div class="panel">
      <div class="fmt-head"><div class="planet">${Cosmos.planet(sel)}</div><div><h2 style="margin:0">${esc(sel.model || "—")}</h2><div class="mono dim">${sel.number != null ? "disk " + sel.number : esc(sel.path)} · ${size(sel.size)} · ${TABLE[sel.table]} · ${esc(sel.partitions.flatMap((p) => p.mounts).join(" ") || "—")}</div></div></div>
      ${strip(sel.partitions, sel.size)}
      <div class="fmt-grid">
        <label>${L("Système de fichiers", "File system")}${sel1("fs", [["ntfs", "NTFS"], ["exfat", "exFAT"], ["fat32", "FAT32"]])}</label>
        <label>${L("Table", "Table")}${sel1("table", [["gpt", "GPT"], ["mbr", "MBR"]])}</label>
        <label>${L("Nom du volume", "Volume name")} <span class="dim">(${f.label.length}/${FS_MAX[f.fs]})</span><input type="text" id="fmt-label" value="${esc(f.label)}" maxlength="${FS_MAX[f.fs]}" spellcheck="false" ${fm.is("running") ? "disabled" : ""} placeholder="${L("sans nom", "no name")}"></label>
      </div>
      ${c && c.label !== f.label && !refused.length ? `<p class="dim">${L("Nom écrit :", "Name written:")} <b class="mono">${esc(c.label)}</b></p>` : ""}
      ${notes.length ? `<ul class="notes">${notes.map((n) => `<li>${esc(n)}</li>`).join("")}</ul>` : ""}
      ${fm.is("checking") ? `<div class="loader"></div>` : refused.length ? findingsHtml(refused.map((r) => ({ severity: "error", title: r, detail: "", blocking: true }))) : ""}
    </div>`;
    let action = "";
    if (fm.is("running")) action = `<div class="panel"><h2>${L("Formatage…", "Formatting…")}</h2><div class="loader"></div><ol class="fmt-steps">${f.steps.map((x) => `<li>${esc(x)}</li>`).join("")}</ol></div>`;
    else if (fm.is("done")) action = `<div class="verdict ok"><div class="badge">${badge("ok")}</div><div><h1>${L("Disque formaté", "Disk formatted")}</h1><ul class="selectable"><li class="mono">${esc(f.outcome.table.toUpperCase())} · ${esc({ ntfs: "NTFS", exfat: "exFAT", fat32: "FAT32" }[f.outcome.fs])} · « ${esc(f.outcome.label || "—")} » · ${size(f.outcome.partition_size)} · ${esc(f.outcome.mount || L("sans lettre", "no letter"))}</li>${f.outcome.steps.map((x) => `<li>${esc(x)}</li>`).join("")}</ul></div></div><div class="footer-bar"><span class="spacer"></span><button class="btn primary" data-act="fmt-again">OK</button></div>`;
    else if (fm.is("failed")) action = `<div class="verdict failed"><div class="badge">${badge("failed")}</div><div><h1>${L("Formatage échoué", "Formatting failed")}</h1><ul><li class="selectable">${esc(f.error)}</li></ul></div></div><div class="footer-bar"><span class="spacer"></span><button class="btn" data-act="fmt-again">OK</button></div>`;
    else if (fm.is("ready")) action = `<div class="danger-zone"><h2 class="dz-title">${L("Irréversible", "Irreversible")}</h2>
      <p class="big">${L(`Tout le contenu de « ${esc(sel.model)} » (${size(sel.size)}) sera effacé.`, `All content of “${esc(sel.model)}” (${size(sel.size)}) will be erased.`)}</p>
      <div class="row wrap" style="margin-top:12px"><span>${L("Saisir", "Type")} <b class="mono">${word}</b> ${L("puis Entrée", "then Enter")}</span><input type="text" id="fmt-confirm" class="${good ? "good" : ""}" value="${esc(f.confirm)}" autocomplete="off" spellcheck="false" maxlength="12"><span class="spacer"></span><button class="btn danger" data-act="fmt-start" ${good ? "" : "disabled"}>${L("Formater", "Format")}</button></div></div>`;
    right = form + action;
  }
  return `<div class="page"><div class="page-head"><div><h1>${L("Formater", "Format")}</h1><p>${L("Disque entier : nouvelle table, une partition, un nom.", "Whole disk: new table, one partition, one name.")}</p></div><button class="btn small ghost" data-act="refresh">↻ ${L("Actualiser", "Refresh")}</button></div>
    <div class="split"><div class="disk-list">${list}</div><div>${right}</div></div></div>`;
}

let fmtCheckGen = 0;
async function formatCheck() {
  const f = S.fmt;
  // Not while formatting; a check already running is simply replaced by this one.
  if (!f.id || !(M.format.can("checking") || M.format.is("checking"))) return;
  const gen = ++fmtCheckGen;
  M.format.to("checking");
  f.outcome = null; f.error = null;
  render();
  let check = null;
  try { check = await api.invoke("format_check", { id: f.id, request: { fs: f.fs, table: f.table, label: f.label } }); } catch (e) { check = { refusals: [typeof e === "string" ? e : String(e)], label: f.label }; }
  if (gen !== fmtCheckGen) return;
  f.check = check;
  M.format.to(check.refusals.length ? "refused" : "ready");
  if (S.view === "format") render();
}

let fmtLabelTimer = null;
function bindFormat() {
  const lab = $("#fmt-label");
  lab?.addEventListener("input", (ev) => {
    S.fmt.label = ev.target.value;
    S.fmt.confirm = "";
    clearTimeout(fmtLabelTimer);
    fmtLabelTimer = setTimeout(() => { formatCheck().then(() => { const l = $("#fmt-label"); if (l) { l.focus(); l.setSelectionRange(l.value.length, l.value.length); } }); }, 350);
  });
  const c = $("#fmt-confirm");
  c?.addEventListener("input", (ev) => {
    S.fmt.confirm = ev.target.value;
    const ok = S.fmt.confirm.trim().toUpperCase() === confirmWord();
    ev.target.classList.toggle("good", ok);
    const b = $('[data-act="fmt-start"]');
    if (b) b.disabled = !ok;
  });
  c?.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter" && S.fmt.confirm.trim().toUpperCase() === confirmWord()) { ev.preventDefault(); actions["fmt-start"](); }
  });
}

// -------------------------------------------------------------- View: settings
// Full logo, next to the title-bar one (the UI bench serves the page from another folder).
const logoUrl = () => ($(".brand img")?.getAttribute("src") || "logo-tiny.svg").replace(/logo-tiny\.svg$/, "logo.svg");

function viewSettings() {
  const e = S.env || {};
  return `<div class="page"><div class="page-head"><div><h1>${L("Réglages", "Settings")}</h1></div></div>
    <div class="panel"><h2>${L("Affichage", "Display")}</h2>
      <div class="opt"><div><label>${L("Langue", "Language")}</label><small>${L("Interface, diagnostics et nouveaux rapports.", "Interface, diagnostics and new reports.")}</small></div>
      <select data-ui="lang"><option value="fr" ${LANG === "fr" ? "selected" : ""}>Français</option><option value="en" ${LANG === "en" ? "selected" : ""}>English</option></select></div>
      <div class="opt"><div><label>Animations</label><small>${L("Le ciel étoilé, le faisceau de transfert et les transitions. « Réduites » fige le fond.", "The starry sky, the transfer beam and the transitions. \"Reduced\" freezes the background.")}</small></div>
      <select data-ui="motion"><option value="full" ${S.ui.motion === "full" ? "selected" : ""}>${L("Complètes", "Full")}</option><option value="reduced" ${S.ui.motion === "reduced" ? "selected" : ""}>${L("Réduites", "Reduced")}</option><option value="off" ${S.ui.motion === "off" ? "selected" : ""}>${L("Aucune", "None")}</option></select></div>
      <div class="opt"><div><label>${L("Empêcher la veille", "Prevent sleep")}</label><small>${L("Tant que Chud Clone est ouvert, le PC ne se met pas en veille. Désactivé : la veille reste bloquée pendant un clonage, un test ou un formatage.", "While Chud Clone is open, the PC does not go to sleep. Off: sleep stays blocked during a clone, a scan or a format.")}</small></div><label class="toggle"><input type="checkbox" data-uicheck="keepAwake" ${S.ui.keepAwake !== false ? "checked" : ""}><span>${L("Activé", "On")}</span></label></div>
      <div class="opt"><div><label>${L("Sons de fin de clonage", "End-of-clone sounds")}</label><small>${L("Son de réussite ou d'échec.", "Success or failure sound.")}</small></div><label class="toggle"><input type="checkbox" data-uicheck="sound" ${S.ui.sound !== false ? "checked" : ""}><span>${L("Activés", "On")}</span></label></div></div>
    ${updatePanel()}
    <div class="panel"><h2>${L("À propos", "About")}</h2><div class="about-head"><img src="${esc(logoUrl())}" alt="" /><div><b>${esc(e.name || "Chud Clone")}</b><span class="muted">${L("Clonage et diagnostic de disques", "Disk cloning and diagnostics")}</span></div></div>${kv([["Version", `${esc(e.name)} ${esc(e.version)}`], ["Mode", e.portable ? L("Portable (données à côté du programme)", "Portable (data next to the program)") : L("Installé", "Installed")], [L("Droits", "Rights"), e.elevated ? L("Administrateur", "Administrator") : L("Limités (lecture des propriétés uniquement)", "Limited (properties only)")], [L("Données", "Data"), `<code>${esc(e.data_dir)}</code>`], [L("Rapports", "Reports"), `<code>${esc(e.reports_dir)}</code>`], [L("Confidentialité", "Privacy"), L("Aucune télémétrie. Seule connexion réseau : la recherche de mises à jour sur GitHub, désactivable ci-dessus.", "No telemetry. Only network connection: the update check on GitHub, which can be turned off above.")]])}</div></div>`;
}

// ------------------------------------------------------------------ Updates
function updateStatus() {
  const u = S.update;
  if (u.checking) return `<span class="muted">${L("Recherche en cours…", "Checking…")}</span>`;
  if (u.error) return `<span style="color:var(--warn)">${esc(u.error)}</span>`;
  if (!u.info) return `<span class="muted">${L("Pas encore vérifié.", "Not checked yet.")}</span>`;
  const when = u.checkedAt ? new Date(u.checkedAt).toLocaleTimeString(LOCALE(), { hour: "2-digit", minute: "2-digit" }) : "";
  if (!u.info.available) return `<span style="color:var(--ok)">${L(`Chud Clone est à jour (vérifié à ${when}).`, `Chud Clone is up to date (checked at ${when}).`)}</span>`;
  return `<b style="color:var(--cyan)">${L(`Version ${esc(u.info.latest)} disponible.`, `Version ${esc(u.info.latest)} is available.`)}</b>`;
}

function updatePanel() {
  const u = S.update, i = u.info;
  const install = i?.available
    ? i.method === "manual"
      ? `<button class="btn primary small" data-act="update-page">${L("Télécharger depuis GitHub", "Download from GitHub")}</button>`
      : `<button class="btn primary small" data-act="update-confirm" ${u.installing ? "disabled" : ""}>${L("Mettre à jour", "Update now")}</button><button class="btn ghost small" data-act="update-page">${L("Notes de version", "Release notes")}</button>`
    : "";
  return `<div class="panel"><h2>${L("Mises à jour", "Updates")}</h2>
      <div class="opt"><div><label>${L("Version installée", "Installed version")}</label><small>${L("Les nouvelles versions sont publiées sur GitHub.", "New versions are published on GitHub.")}</small></div>
      <div><div style="margin-bottom:8px">${esc(S.env?.version || "")} · ${updateStatus()}</div><div class="row wrap"><button class="btn small" data-act="update-check" ${u.checking || u.installing ? "disabled" : ""}>${L("Rechercher maintenant", "Check now")}</button>${install}</div></div></div>
      <div class="opt"><div><label>${L("Recherche automatique", "Automatic check")}</label><small>${L("Au démarrage, une seule requête vers GitHub. Aucune information sur l'ordinateur ou les disques n'est envoyée.", "At startup, a single request to GitHub. No information about the computer or the disks is sent.")}</small></div>
      <label class="toggle"><input type="checkbox" data-uicheck="autoUpdate" ${S.ui.autoUpdate !== false ? "checked" : ""}><span>${L("Rechercher les mises à jour au démarrage", "Check for updates at startup")}</span></label></div></div>`;
}

async function checkUpdate(manual) {
  const u = S.update;
  if (!M.update.can("checking")) return;
  M.update.to("checking"); u.error = null;
  if (S.view === "settings") render();
  try {
    u.info = await api.invoke("update_check");
    u.checkedAt = Date.now();
    if (manual && !u.info.available) toast(L("Chud Clone est à jour.", "Chud Clone is up to date."));
    if (!manual && u.info.available && S.ui.skipVersion !== u.info.latest) showUpdateToast();
  } catch (e) {
    u.error = typeof e === "string" ? e : String(e);
    if (manual) fail(e);
  }
  M.update.to("idle");
  if (S.view === "settings") render();
}

function showUpdateToast() {
  $("#update-toast")?.remove();
  const t = document.createElement("div");
  t.id = "update-toast";
  t.className = "update-note";
  t.setAttribute("role", "status");
  t.innerHTML = `<div><b>${L(`Chud Clone ${esc(S.update.info.latest)} est disponible`, `Chud Clone ${esc(S.update.info.latest)} is available`)}</b><br><span class="muted">${L(`Version actuelle : ${esc(S.update.info.current)}.`, `Current version: ${esc(S.update.info.current)}.`)}</span></div>
    <div class="row wrap"><button class="btn primary small" data-act="update-confirm">${L("Voir", "View")}</button><button class="btn ghost small" data-act="update-later">${L("Plus tard", "Later")}</button><button class="btn ghost small" data-act="update-skip">${L("Ignorer cette version", "Skip this version")}</button></div>`;
  document.body.appendChild(t);
}

// Release notes as plain text (Markdown images and markup stripped).
const releaseNotes = (md) => String(md || "").replace(/!\[[^\]]*\]\([^)]*\)\s*/g, "").replace(/\*\*([^*]+)\*\*/g, "$1").replace(/\r/g, "").trim();

function updateConfirm() {
  const i = S.update.info;
  if (!i) return;
  $("#update-toast")?.remove();
  const manual = i.method === "manual";
  const how = i.method === "installer"
    ? L("Le paquet est vérifié avec la somme de contrôle SHA-256 publiée, puis installé dans le même dossier. Windows demandera l'autorisation administrateur. Chud Clone se ferme puis redémarre sur la nouvelle version ; vos réglages et rapports sont conservés.", "The package is checked against the published SHA-256 checksum, then installed in the same folder. Windows will ask for administrator permission. Chud Clone closes and restarts on the new version; your settings and reports are kept.")
    : i.method === "portable"
      ? L("L'archive est vérifiée avec la somme de contrôle SHA-256 publiée, puis les programmes de ce dossier sont remplacés. Le dossier ChudClone-data (réglages, rapports) est conservé. Chud Clone redémarre ensuite.", "The archive is checked against the published SHA-256 checksum, then the programs in this folder are replaced. The ChudClone-data folder (settings, reports) is kept. Chud Clone then restarts.")
      : L("Cette copie de Chud Clone ne peut pas être mise à jour automatiquement : téléchargez la nouvelle version depuis la page de publication.", "This copy of Chud Clone cannot be updated automatically: download the new version from the release page.");
  const notes = releaseNotes(i.notes);
  modal(`<h2>${esc(i.title)}</h2><p>${L(`Version installée : ${esc(i.current)}. Nouvelle version : <b>${esc(i.latest)}</b>`, `Installed version: ${esc(i.current)}. New version: <b>${esc(i.latest)}</b>`)}${i.download_size ? ` (${size(i.download_size)})` : ""}.</p>
    <p class="muted">${how}</p>${notes ? `<div class="notes">${esc(notes)}</div>` : ""}
    <div class="actions"><button class="btn ghost" data-act="modal-close">${L("Plus tard", "Later")}</button><button class="btn" data-act="update-page">${L("Page de publication", "Release page")}</button>${manual ? "" : `<button class="btn primary" data-act="update-install">${L("Installer maintenant", "Install now")}</button>`}</div>`);
}

function updateProgress(p) {
  const bar = $("#update-bar"), txt = $("#update-text");
  if (!bar || !txt) return;
  if (p.phase === "download" && p.total) {
    bar.classList.remove("busy");
    bar.firstElementChild.style.width = Math.min(100, (100 * p.done) / p.total).toFixed(1) + "%";
    txt.textContent = L(`Téléchargement : ${size(p.done)} sur ${size(p.total)}`, `Downloading: ${size(p.done)} of ${size(p.total)}`);
  } else {
    bar.classList.add("busy");
    txt.textContent = p.phase === "verify" ? L("Vérification de la somme de contrôle…", "Verifying the checksum…") : p.phase === "install" ? L("Installation… Chud Clone va redémarrer.", "Installing… Chud Clone will restart.") : L("Téléchargement…", "Downloading…");
  }
}

async function updateInstall() {
  if (!M.update.can("installing") || busy()) return;
  M.update.to("installing");
  modal(`<h2>${L("Mise à jour de Chud Clone", "Updating Chud Clone")}</h2><p class="muted">${L(`Version ${esc(S.update.info.latest)}.`, `Version ${esc(S.update.info.latest)}.`)}</p>
    <div class="ubar busy" id="update-bar"><i></i></div><p id="update-text" class="dim">${L("Connexion…", "Connecting…")}</p>`);
  try { await api.invoke("update_install"); } catch (e) { M.update.to("idle"); closeModal(); fail(e); }
}

// -------------------------------------------------------------------- Render
// Short, subtle "glitch" transition of the new page: a slight horizontal jitter with a
// thin chromatic fringe, a faint split of the title and a few dim bands sweeping the
// area. Everything is removed at the end: a later render of the
// same screen replays nothing.
let glitchTimer = 0;
let glitchStart = -1;
const GLITCH_MS = 450;
// Splits the title; `elapsed` resumes the animation where it was if the page was
// redrawn during the transition.
function glitchTitle(view, elapsed) {
  const h = view.querySelector(".page h1");
  if (!h || document.documentElement.classList.contains("reduced-motion")) return;
  h.dataset.text = h.textContent;
  if (elapsed) h.style.setProperty("--gd", -elapsed + "ms");
  h.classList.add("glitch-text");
}
function glitchIn(view) {
  const root = document.documentElement;
  if (root.classList.contains("no-motion")) return;
  clearTimeout(glitchTimer);
  document.getElementById("glitch-fx")?.remove();
  view.classList.remove("glitch-in");
  void view.offsetWidth;
  view.classList.add("glitch-in");
  glitchStart = performance.now();
  glitchTitle(view, 0);
  const reduced = root.classList.contains("reduced-motion");
  let fx = null;
  if (!reduced) {
    const r = view.getBoundingClientRect();
    fx = document.createElement("div");
    fx.id = "glitch-fx";
    Object.assign(fx.style, { left: r.left + "px", top: r.top + "px", width: r.width + "px", height: r.height + "px" });
    const colors = ["rgba(79,227,255,.22)", "rgba(255,79,130,.18)", "rgba(154,107,255,.18)"];
    for (let i = 0; i < 3; i++) {
      const b = document.createElement("i");
      const w = 25 + Math.random() * 50;
      Object.assign(b.style, {
        top: Math.random() * 92 + "%", height: 1 + Math.random() * 5 + "px", width: w + "%", left: Math.random() * (100 - w) + "%",
        background: colors[i % colors.length], animationDelay: Math.round(Math.random() * 80) + "ms",
      });
      b.style.setProperty("--dx", (Math.random() < 0.5 ? -1 : 1) * (4 + Math.random() * 14) + "px");
      fx.appendChild(b);
    }
    document.body.appendChild(fx);
  }
  glitchTimer = setTimeout(() => {
    glitchStart = -1;
    view.classList.remove("glitch-in");
    view.querySelectorAll(".glitch-text").forEach((x) => x.classList.remove("glitch-text"));
    fx?.remove();
  }, GLITCH_MS);
}

function render() {
  const keep = {};
  document.querySelectorAll("details[data-keep]").forEach((d) => (keep[d.dataset.keep] = d.open));
  if ("adv" in keep) S.advOpen = keep.adv;
  if ("log" in keep) S.logOpen = keep.log;
  const view = $("#view");
  const scroll = view.scrollTop;
  const screen = S.view + ":" + (S.view === "clone" ? S.step : S.view === "reports" ? S.reportOpen?.id || "" : "");
  const sameScreen = view.dataset.screen === screen;
  if (beam) { beam.destroy(); beam = null; }
  if (star) { star.destroy(); star = null; }
  document.documentElement.dataset.currentView = S.view;
  document.documentElement.dataset.activity = activity();
  view.innerHTML = { clone: viewClone, disks: viewDisks, format: viewFormat, reports: viewReports, settings: viewSettings }[S.view]();
  view.dataset.screen = screen;
  view.scrollTop = sameScreen ? scroll : 0;
  // Transition only when the screen changes: refreshes (loading, SMART, progress)
  // replace the content without replaying anything.
  if (!sameScreen) glitchIn(view);
  else if (glitchStart >= 0) glitchTitle(view, performance.now() - glitchStart);
  document.querySelectorAll(".titlebar nav button").forEach((b) => b.classList.toggle("active", b.dataset.view === S.view));
  const a = $("#admin");
  if (S.env) {
    a.className = "admin " + (S.env.elevated ? "full" : "limited");
    a.innerHTML = `<span class="dot"></span>${S.env.elevated ? L("Administrateur", "Administrator") : L("Mode limité — passer en administrateur", "Limited mode — switch to administrator")}`;
    a.disabled = S.env.elevated;
  }
  if (S.view === "disks") drawScanMap();
  if (S.view === "clone" && S.step === "run") {
    beam = new Cosmos.Beam($("#beam"));
    star = new Cosmos.Star($("#star"));
    const pre = $("#log");
    for (const e of S.run.logs) {
      const line = document.createElement("div");
      line.innerHTML = `<span class="t">${esc(e.time)}</span> <span class="${e.level === "warning" ? "w" : e.level === "error" ? "e" : ""}">${esc(e.message)}</span>`;
      pre.appendChild(line);
    }
    updateRun();
  }
  if (S.view === "clone" && S.step === "review") {
    const c = $("#confirm");
    c?.addEventListener("input", (ev) => {
      S.confirmText = ev.target.value;
      const ok = S.confirmText.trim().toUpperCase() === confirmWord();
      ev.target.classList.toggle("good", ok);
      const b = $('[data-act="start"]');
      if (b) b.disabled = !ok;
    });
    // Enter starts the clone once the word is right.
    c?.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter" && S.confirmText.trim().toUpperCase() === confirmWord()) { ev.preventDefault(); actions.start(); }
    });
    $("#opt-shutdown")?.addEventListener("change", (ev) => { S.shutdown = ev.target.checked; });
  }
  if (S.view === "format") bindFormat();
}

async function loadDisks(silent) {
  if (!silent) { S.loadingDisks = true; render(); }
  try {
    S.disks = await api.invoke("list_disks");
  } catch (e) { fail(e); }
  S.loadingDisks = false;
  if (S.srcId && !src()) S.srcId = null;
  if (S.dstId && !dst()) S.dstId = null;
  render();
  if (S.view === "disks") surveyHealth();
}

function params(dstId, prep) {
  const o = S.opts;
  return {
    source_id: S.srcId, destination_id: dstId, mode: o.mode, optimize_layout: !!o.optimize_layout, grow_main: o.grow_main !== false, verify: o.verify, unreadable: o.unreadable,
    read_retries: Number(o.read_retries), allow_unvalidated_physical_write: false,
    source_fingerprint: prep?.source.fingerprint ?? null, destination_fingerprint: prep?.destination.fingerprint ?? null,
    eject: !!o.eject, boot_prep: o.boot_prep !== false,
  };
}

// Shows the analysis of destination i of the queue.
function selectPrep(i) {
  S.prepIdx = Math.max(0, Math.min(i, S.dstIds.length - 1));
  const x = S.preps[S.prepIdx] || {};
  S.dstId = S.dstIds[S.prepIdx] || null;
  S.prep = x.prep || null;
  S.prepError = x.error || null;
  S.prepBusy = !!x.busy;
}

// Analyses every destination of the queue, one after another. Writes nothing.
async function prepare() {
  const gen = (S.prepGen = (S.prepGen || 0) + 1);
  S.preps = S.dstIds.map(() => ({ busy: true }));
  S.confirmText = "";
  S.prepLang = LANG;
  selectPrep(S.prepIdx);
  render();
  await langReady;
  for (let i = 0; i < S.dstIds.length; i++) {
    let x;
    try {
      const prep = await api.invoke("prepare_clone", { params: params(S.dstIds[i], null) });
      S.smart[prep.source.id] = prep.smart_source;
      S.smart[prep.destination.id] = prep.smart_destination;
      x = { prep };
    } catch (e) { x = { error: typeof e === "string" ? e : e?.message || String(e) }; }
    if (gen !== S.prepGen) return;
    S.preps[i] = x;
    selectPrep(S.prepIdx);
    if (S.view === "clone" && S.step === "review") render();
  }
}

// ------------------------------------------------------------ Clone queue
async function startQueueItem() {
  const q = S.queue, it = q.items[q.idx];
  S.run = { source: it.prep.source, destination: it.prep.destination, progress: null, logs: [], started: Date.now(), err: { read: 0, bad: 0, rec: 0, write: 0 } };
  S.report = null; S.runError = null; S.runBlocked = null;
  try {
    await api.invoke("start_clone", { params: params(it.id, it.prep), confirmation: q.word });
    M.clone.to("run");
    render();
  } catch (e) {
    // Nothing started at all (first clone refused): stay on the review, say why.
    if (M.clone.is("review")) { S.queue = null; S.run = null; fail(e); render(); return; }
    queueDone({ report: null, error: typeof e === "string" ? e : e?.message || String(e), blocked: [] });
  }
}

// End of one clone of the queue: next one, or the final result.
function queueDone(payload) {
  const q = S.queue;
  if (!q) return;
  const it = q.items[q.idx];
  Object.assign(it, { done: true, report: payload.report, error: payload.error, blocked: payload.blocked, ejected: payload.ejected ?? null, ejectError: payload.eject_error || null });
  if (it.ejected === false) toast(L("Éjection impossible : ", "Eject failed: ") + (it.ejectError || ""), true);
  if (q.idx + 1 < q.items.length) {
    q.idx++;
    startQueueItem();
    return;
  }
  const allGood = q.items.every(itemGood);
  playSound(allGood);
  const firstBad = q.items.findIndex((x) => !itemGood(x));
  showResult(firstBad >= 0 ? firstBad : q.items.length - 1);
  M.clone.to("result"); S.run = null;
  render();
  loadDisks(true);
  if (q.shutdown) shutdownCountdown();
}

function showResult(i) {
  const it = S.queue.items[i];
  S.resultIdx = i;
  S.report = it.report; S.runError = it.error; S.runBlocked = it.blocked;
}

function soundUrl(name) {
  return ($(".brand img")?.getAttribute("src") || "logo-tiny.svg").replace(/logo-tiny\.svg$/, "sounds/" + name + ".mp3");
}
function playSound(good) {
  if (S.ui.sound === false) return;
  try { const a = new Audio(soundUrl(good ? "good" : "bad")); a.volume = 0.9; a.play().catch(() => {}); } catch { /* no audio */ }
}

let shutdownTimer = null;
function shutdownCountdown() {
  if (!M.shutdown.can("counting")) return;
  M.shutdown.to("counting");
  let left = 60;
  const draw = () => modal(`<h2 style="color:var(--warn)">${L("Extinction du PC", "Shutting down")}</h2><p class="big mono" style="font-size:40px;margin:10px 0">${left} s</p><p class="muted">${L("Rapports enregistrés.", "Reports saved.")}</p>
    <div class="actions"><button class="btn primary" data-act="shutdown-cancel">${L("Annuler l'extinction", "Cancel shutdown")}</button><button class="btn danger" data-act="shutdown-now">${L("Éteindre maintenant", "Shut down now")}</button></div>`);
  draw();
  shutdownTimer = setInterval(() => {
    left--;
    if (left <= 0) return void actions["shutdown-now"]();
    draw();
  }, 1000);
}

// ----------------------------------------------------------------- Actions
const actions = {
  pick(el) {
    const id = el.dataset.id;
    if (S.step === "source") { S.srcId = id; S.dstIds = S.dstIds.filter((x) => x !== id); }
    else S.dstIds = S.dstIds.includes(id) ? S.dstIds.filter((x) => x !== id) : [...S.dstIds, id];
    S.dstId = S.dstIds[0] || null;
    render();
  },
  next() {
    if (M.clone.is("source") && S.srcId) M.clone.to("destination");
    else if (M.clone.is("destination") && S.dstIds.length) { M.clone.to("review"); S.prepIdx = 0; prepare(); return; }
    render();
  },
  "prep-tab"(el) { selectPrep(Number(el.dataset.i)); render(); },
  "result-sel"(el) { showResult(Number(el.dataset.i)); render(); },
  "shutdown-cancel"() {
    if (!M.shutdown.is("counting")) return;
    clearInterval(shutdownTimer); shutdownTimer = null; closeModal();
    M.shutdown.to("idle");
    toast(L("Extinction annulée.", "Shutdown cancelled."));
  },
  async "shutdown-now"() {
    if (!M.shutdown.can("requested")) return;
    clearInterval(shutdownTimer); shutdownTimer = null; closeModal();
    M.shutdown.to("requested");
    try { await api.invoke("shutdown_pc"); } catch (e) { M.shutdown.to("idle"); fail(e); }
  },
  back() {
    if (M.clone.is("review")) M.clone.to("destination");
    else if (M.clone.is("destination")) M.clone.to("source");
    render();
  },
  refresh() { loadDisks(); },
  reprepare() { prepare(); },
  async "add-image"() {
    try {
      const path = await api.open({ title: L("Choisir un fichier image disque", "Choose a disk image file"), multiple: false, filters: [{ name: L("Images disque", "Disk images"), extensions: ["img", "bin", "raw", "dd", "ima"] }, { name: L("Tous les fichiers", "All files"), extensions: ["*"] }] });
      if (!path) return;
      const info = await api.invoke("add_image", { path });
      await loadDisks(true);
      if (S.step === "source") S.srcId = info.id; else if (info.id !== S.srcId && !S.dstIds.includes(info.id)) S.dstIds.push(info.id);
      S.dstId = S.dstIds[0] || null;
      render();
    } catch (e) { fail(e); }
  },
  async "new-image"() {
    const s = src();
    if (!s) return;
    try {
      const path = await api.save({ title: L("Enregistrer l'image de destination", "Save the destination image"), defaultPath: "clone.img", filters: [{ name: L("Image disque", "Disk image"), extensions: ["img"] }] });
      if (!path) return;
      // No size: the image gets the size the source's partitions need.
      const info = await api.invoke("add_new_image", { path, size: null, sourceId: s.id });
      await loadDisks(true);
      if (!S.dstIds.includes(info.id)) S.dstIds.push(info.id);
      S.dstId = S.dstIds[0];
      render();
    } catch (e) { fail(e); }
  },
  async start() {
    const ready = S.preps.map((x, i) => ({ id: S.dstIds[i], prep: x.prep, state: prepState(x) })).filter((x) => x.state === "ready");
    // Only from the review, only once, and never while something else is running.
    if (!ready.length || !M.clone.is("review") || S.queue) return;
    const word = S.confirmText.trim().toUpperCase();
    if (word !== confirmWord() || busy()) return;
    S.queue = { items: ready, idx: 0, word, eject: !!S.opts.eject, shutdown: S.shutdown };
    await startQueueItem();
  },
  cancel() {
    modal(`<h2>${L("Annuler le clonage ?", "Cancel cloning?")}</h2><p>${L("La copie s'arrêtera immédiatement. <b>L'ancien contenu de la destination est déjà perdu</b> et ne sera pas restauré : la destination restera sans table de partitions, inutilisable jusqu'à un nouveau clonage complet.", "The copy will stop immediately. <b>The destination's previous content is already lost</b> and will not be restored: the destination will stay without a partition table, unusable until a new complete clone.")}</p><p class="muted">${L("La source n'est pas modifiée.", "The source is not modified.")}</p>
      <div class="actions"><button class="btn primary" data-act="modal-close">${L("Continuer le clonage", "Keep cloning")}</button><button class="btn danger" data-act="cancel-yes">${L("Annuler le clonage", "Cancel cloning")}</button></div>`);
  },
  "cancel-yes"() { closeModal(); api.invoke("cancel_clone"); const b = $("#btn-cancel"); if (b) { b.disabled = true; b.textContent = L("Annulation en cours…", "Cancelling…"); } },
  "modal-close"() { closeModal(); },
  "update-check"() { checkUpdate(true); },
  "update-confirm"() { updateConfirm(); },
  "update-install"() { updateInstall(); },
  "update-page"() { api.invoke("open_release_page").catch(fail); },
  "update-later"() { $("#update-toast")?.remove(); },
  "update-skip"() { S.ui.skipVersion = S.update.info?.latest || null; store.set("ui", S.ui); $("#update-toast")?.remove(); },
  "unreadable-stop"() { closeModal(); api.invoke("answer_unreadable", { zeroFill: false }); },
  "unreadable-zero"() { closeModal(); api.invoke("answer_unreadable", { zeroFill: true }); },
  restart() { if (!M.clone.can("source")) return; M.clone.to("source"); S.prep = null; S.preps = []; S.prepIdx = 0; S.queue = null; S.report = null; S.runError = null; S.run = null; S.confirmText = ""; S.dstId = null; S.dstIds = []; S.shutdown = false; loadDisks(); },
  async export(el) {
    const fmt = el.dataset.fmt, id = el.dataset.id;
    try {
      const path = await api.save({ title: L("Exporter le rapport", "Export the report"), defaultPath: `${L("Rapport Chud Clone", "Chud Clone report")} ${id}.${fmt}`, filters: [{ name: fmt.toUpperCase(), extensions: [fmt] }] });
      if (!path) return;
      await api.invoke("export_report", { id, format: fmt, path });
      toast(L("Rapport exporté : ", "Report exported: ") + path);
    } catch (e) { fail(e); }
  },
  "disk-sel"(el) { S.diskSel = el.dataset.id; render(); },
  "fmt-sel"(el) {
    if (M.format.is("running")) return;
    Object.assign(S.fmt, { id: el.dataset.id, check: null, confirm: "", steps: [] });
    formatCheck();
  },
  "fmt-again"() { Object.assign(S.fmt, { steps: [], confirm: "" }); formatCheck(); },
  async "fmt-start"() {
    const f = S.fmt;
    if (!M.format.can("running") || f.confirm.trim().toUpperCase() !== confirmWord() || busy()) return;
    M.format.to("running");
    Object.assign(f, { steps: [], outcome: null, error: null });
    render();
    try {
      f.outcome = await api.invoke("format_disk", { id: f.id, fingerprint: f.check.disk.fingerprint, request: { fs: f.fs, table: f.table, label: f.label }, confirmation: f.confirm.trim().toUpperCase() });
      M.format.to("done");
    } catch (e) {
      f.error = typeof e === "string" ? e : e?.message || String(e);
      M.format.to("failed");
    }
    f.confirm = "";
    render();
    loadDisks(true);
  },
  async smart(el) {
    await readHealth(el.dataset.id);
  },
  async fixboot(el) {
    const id = el.dataset.id;
    if (busy()) return;
    S.fix[id] = { busy: true, steps: [] }; S.fixing = id; render();
    try {
      S.fix[id].result = await api.invoke("fix_boot", { id, fingerprint: S.disks.find((d) => d.id === id)?.fingerprint ?? "" });
      // The BCD and the volumes changed: read the boot elements again.
      try { S.inspect[id] = await api.invoke("inspect_disk", { id }); } catch { /* the previous analysis stays */ }
    } catch (e) { S.fix[id].error = typeof e === "string" ? e : String(e); }
    S.fix[id].busy = false; S.fixing = null; render();
  },
  async inspect(el) {
    const id = el.dataset.id;
    S.busy["inspect:" + id] = true; render();
    try { S.inspect[id] = await api.invoke("inspect_disk", { id }); } catch (e) { S.inspect[id] = { error: typeof e === "string" ? e : String(e) }; }
    delete S.busy["inspect:" + id]; render();
  },
  async scan(el) {
    const id = el.dataset.id, d = disk(id);
    if (!d) return;
    if (!M.scan.can("running") || busy()) return;
    try {
      S.scan = newScan(id, new Array(Math.min(2400, Math.max(1, Math.ceil(d.size / 1048576)))).fill(255));
      M.scan.to("running");
      await api.invoke("start_scan", { id });
    } catch (e) { M.scan.to("idle"); S.scan = null; fail(e); }
    render();
  },
  "scan-cancel"() { if (S.scan && M.scan.can("cancelling")) { M.scan.to("cancelling"); api.invoke("cancel_scan"); render(); } },
  async "report-open"(el) {
    try { S.reportOpen = await api.invoke("get_report", { id: el.dataset.id }); render(); } catch (e) { fail(e); }
  },
  "report-close"() { S.reportOpen = null; render(); },
  async elevate() {
    try { await api.invoke("relaunch_elevated"); } catch (e) { fail(e); }
  },
};

// Health reading (read-only command) + history: every reading is kept so that
// what gets worse can be flagged later.
async function readHealth(id, quiet) {
  S.busy["smart:" + id] = true;
  if (!quiet) render();
  try {
    const h = await api.invoke("disk_health", { id });
    S.smart[id] = h.smart;
    S.health[id] = { history: h.history, trends: h.trends };
  } catch (e) { if (!quiet) fail(e); }
  delete S.busy["smart:" + id];
  if (S.view === "disks") render();
}

// On the first visit to "Disks & health", reads the health of every disk.
async function surveyHealth() {
  if (S.surveyed) return;
  S.surveyed = true;
  for (const d of S.disks.filter((x) => x.kind === "physical")) {
    if (!S.smart[d.id]) await readHealth(d.id, true);
  }
}

async function goto(view) {
  // A clone and a format are followed on their own screen until they end.
  const home = { cloning: "clone", formatting: "format" }[activity()];
  if (home && view !== home) { toast(BUSY()[activity()]); return; }
  S.view = view;
  // Analysis shown in another language: it is redone.
  if (view === "clone" && S.step === "review" && !S.prepBusy && S.prepLang !== LANG) return void prepare();
  if (view === "reports") { S.reportOpen = null; S.reports = null; render(); try { S.reports = await api.invoke("list_reports"); } catch (e) { S.reports = []; fail(e); } }
  render();
  if (view === "disks" && !S.loadingDisks) surveyHealth();
  if (view === "format" && S.fmt.id) formatCheck();
}

document.addEventListener("click", (ev) => {
  const nav = ev.target.closest("[data-view]");
  if (nav) return void goto(nav.dataset.view);
  const el = ev.target.closest("[data-act]");
  if (el && !el.disabled && actions[el.dataset.act]) actions[el.dataset.act](el, ev);
});
document.addEventListener("change", (ev) => {
  const t = ev.target;
  if (t.dataset.opt) {
    S.opts[t.dataset.opt] = t.type === "checkbox" ? t.checked : t.value;
    store.set("opts", S.opts);
    // An option that changes the plan requires redoing the displayed analysis.
    if (S.view === "clone" && S.step === "review" && ["mode", "optimize_layout", "grow_main"].includes(t.dataset.opt)) prepare();
  }
  if (t.dataset.fmt) {
    S.fmt[t.dataset.fmt] = t.value;
    S.fmt.confirm = "";
    if (t.dataset.fmt === "fs") S.fmt.label = S.fmt.label.slice(0, FS_MAX[S.fmt.fs]);
    formatCheck();
  }
  if (t.dataset.uicheck) {
    S.ui[t.dataset.uicheck] = t.checked;
    store.set("ui", S.ui);
    if (t.dataset.uicheck === "keepAwake") api.invoke("set_keep_awake", { enabled: t.checked }).catch(fail);
  }
  if (t.dataset.ui) {
    S.ui[t.dataset.ui] = t.value;
    store.set("ui", S.ui);
    if (t.dataset.ui === "lang") applyLang(t.value, true);
    else Cosmos.setQuality(S.ui.motion);
  }
});
document.addEventListener("keydown", (ev) => { if (ev.key === "Escape" && $("#modal-root .overlay") && !$('[data-act="unreadable-stop"]') && !S.update.installing) closeModal(); });

// Word to type to confirm erasing (the engine accepts both).
const confirmWord = () => L("EFFACER", "ERASE");

// Changes language: labels, fixed page texts and engine messages. What the engine
// already produced (analyses, health) is read again to be shown in the new language.
function applyLang(lang, reload) {
  LANG = lang === "en" ? "en" : "fr";
  document.documentElement.lang = LANG;
  buildTables();
  document.querySelectorAll("[data-fr]").forEach((el) => { el.textContent = LANG === "en" ? el.dataset.en : el.dataset.fr; });
  document.querySelectorAll("[data-afr]").forEach((el) => el.setAttribute("aria-label", LANG === "en" ? el.dataset.aen : el.dataset.afr));
  document.title = "Chud Clone";
  langReady = api.invoke("set_language", { lang: LANG }).catch(() => {});
  if (!reload) return;
  S.smart = {}; S.health = {}; S.inspect = {}; S.surveyed = false; S.reports = null; S.reportOpen = null;
  loadDisks(true);
  if (S.view === "reports") goto("reports");
  render();
}

// ------------------------------------------------------------------- Startup
async function boot() {
  applyLang(S.ui.lang || (/^fr/i.test(navigator.language || "") ? "fr" : "en"));
  Cosmos.startSky($("#cosmos"), S.ui.motion);
  const w = api.win();
  $("#win-min").onclick = () => w.minimize();
  $("#win-max").onclick = () => w.toggleMaximize();
  $("#win-close").onclick = () => w.close();
  $("#admin").onclick = () => actions.elevate();
  await api.listen("clone-progress", (e) => { if (S.run) { S.run.progress = e.payload; updateRun(); } });
  await api.listen("clone-log", (e) => pushLog(e.payload));
  await api.listen("clone-ask", (e) => askUnreadable(e.payload));
  await api.listen("clone-done", (e) => {
    closeModal();
    const { report } = e.payload;
    const st = report ? STATUS[report.outcome.status][0] : "failed";
    // Lets the final state of the beam show before the next clone or the report.
    if (beam) beam.set({ ok: "ok", warn: "warn", incomplete: "incomplete", failed: "fail" }[st], 0.3, false);
    // What is left of the star settles: a brown dwarf after a good clone.
    if (S.run) S.run.ended = st;
    updateRun();
    setTimeout(() => queueDone(e.payload), document.documentElement.classList.contains("no-motion") ? 0 : 2600);
  });
  await api.listen("format-step", (e) => { S.fmt.steps.push(e.payload); if (S.view === "format") render(); });
  await api.listen("fixboot-step", (e) => { const f = S.fix[S.fixing]; if (f) { f.steps.push(e.payload); if (S.view === "disks") render(); } });
  await api.listen("scan-progress", (e) => {
    const sc = S.scan, p = e.payload;
    if (!sc || !sc.running) return;
    sc.progress = p;
    for (const [i, c] of p.cells) { sc.cells[i] = c; sc.cursor = i; }
    refreshScanPanel();
  });
  await api.listen("scan-done", (e) => {
    const { id, result, error, saved } = e.payload;
    if (!S.scan || S.scan.id !== id) S.scan = newScan(id, []);
    Object.assign(S.scan, { result, error, saved, cells: result ? result.cells : S.scan.cells || [] });
    M.scan.to(result ? "done" : "failed");
    if (result?.bad_sectors) toast(L(`Scan terminé : ${num(result.bad_sectors)} secteur(s) illisible(s).`, `Scan finished: ${num(result.bad_sectors)} unreadable sector(s).`), true);
    else if (result) toast(L("Scan de surface terminé.", "Surface scan finished."));
    if (S.view === "disks") render();
  });
  await api.listen("close-blocked", () => toast(activity() === "formatting" ? L("Un formatage est en cours : attendez qu'il se termine.", "A format is running: wait until it has finished.") : L("Un clonage est en cours. Annulez-le d'abord si vous voulez vraiment fermer.", "A clone is running. Cancel it first if you really want to close."), true));
  await api.listen("update-progress", (e) => updateProgress(e.payload));
  await api.listen("update-failed", (e) => {
    M.update.to("idle");
    closeModal();
    toast(L(`Mise à jour impossible : ${e.payload}`, `Update failed: ${e.payload}`), true);
    if (S.view === "settings") render();
  });
  setInterval(() => { if (S.step === "run" && S.run) updateRun(); }, 1000);
  try { S.env = await api.invoke("env_info"); } catch (e) { fail(e); }
  // No sleep while the application is open, unless turned off in Settings.
  api.invoke("set_keep_awake", { enabled: S.ui.keepAwake !== false }).catch(() => {});
  // No saved choice: Windows language.
  if (!S.ui.lang && S.env?.system_lang) applyLang(S.env.system_lang);
  render();
  await loadDisks();
  // Quiet update check shortly after startup (can be turned off in Settings).
  if (S.ui.autoUpdate !== false) setTimeout(() => checkUpdate(false), 3000);
}
boot();
