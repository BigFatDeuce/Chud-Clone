// Chud Clone — logique de l'interface.
// L'interface ne décide de rien de dangereux : elle affiche ce que le moteur a analysé et
// lui transmet les choix de l'utilisateur. Toutes les protections sont refaites côté moteur.
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
  set(k, v) { try { localStorage.setItem("chudclone." + k, JSON.stringify(v)); } catch { /* stockage indisponible */ } },
};

// ------------------------------------------------------------------- Langue
// Deux langues : français et anglais. `L("texte", "text")` rend le texte de la langue
// active ; les tables de libellés sont reconstruites par applyLang().
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
    ["writing_table", L("Table de partitions", "Partition table")],
    ["verifying", L("Vérification", "Verifying")],
    ["growing", L("Agrandissement", "Growing")],
    ["checking", L("Contrôle final", "Final check")],
  ];
  PHASE_LABEL = {
    preparing: L("Préparation de la destination", "Preparing the destination"),
    copying: L("Copie des données", "Copying data"),
    recovering: L("Nouvelle lecture des zones difficiles", "Re-reading difficult areas"),
    writing_table: L("Écriture de la table de partitions", "Writing the partition table"),
    verifying: L("Vérification des données copiées", "Verifying copied data"),
    growing: L("Agrandissement de la partition Windows", "Growing the Windows partition"),
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

// --------------------------------------------------------------------- État
const S = {
  env: null,
  disks: [],
  loadingDisks: true,
  view: "clone",
  step: "source",
  srcId: null,
  dstId: null,
  prep: null,
  prepBusy: false,
  prepError: null,
  confirmText: "",
  opts: store.get("opts", { mode: "smart", optimize_layout: true, grow_main: true, verify: "hash", unreadable: "ask", read_retries: 3, allow_unvalidated: false }),
  ui: store.get("ui", { motion: "full", lang: null }),
  run: null,
  report: null,
  runError: null,
  reports: null,
  reportOpen: null,
  diskSel: null,
  inspect: {},
  smart: {},
  health: {},
  busy: {},
};
let beam = null;

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
  c.push(`<span class="chip">${esc(d.bus)}</span>`);
  if (d.media !== "unknown" && !(d.kind === "image" && d.media === "image")) c.push(`<span class="chip">${MEDIA[d.media] || esc(d.media)}</span>`);
  c.push(`<span class="chip">${TABLE[d.table]}</span>`);
  if (d.is_system) c.push(`<span class="chip sys">${L("Windows en cours", "Running Windows")}</span>`);
  if (d.removable) c.push(`<span class="chip">${L("Amovible", "Removable")}</span>`);
  if (d.read_only) c.push(`<span class="chip warn">${L("Lecture seule", "Read-only")}</span>`);
  if (d.offline) c.push(`<span class="chip warn">${L("Hors ligne", "Offline")}</span>`);
  const sm = S.smart[d.id];
  if (sm?.available) c.push(`<span class="chip ${sm.level === "good" ? "ok" : sm.level === "watch" ? "warn" : "bad"}">${L("Santé : ", "Health: ")}${HEALTH[sm.level].toLowerCase()}</span>`);
  return c.join("") + (extra || "");
}

function diskCard(d, o) {
  o = o || {};
  const used = d.used_bytes;
  const mounts = d.partitions.flatMap((p) => p.mounts).map((m) => m.replace(/\\$/, "")).join(" ");
  const sub = [d.number != null ? `${L("Disque", "Disk")} ${d.number}` : L("Fichier image", "Image file"), mounts].filter(Boolean).join(" · ");
  const cls = ["disk", o.mini ? "mini" : "", o.selected ? "selected" : "", o.dest ? "dest" : "", o.locked ? "locked" : ""].join(" ");
  const tag = o.tag ? `<span class="role-tag${o.dest ? " dest" : ""}">${o.tag}</span>` : "";
  const inner = `${tag}<div class="planet">${Cosmos.planet(d)}</div><div style="min-width:0">
      <h3 title="${esc(d.model)}">${esc(d.model || L("Disque sans nom", "Unnamed disk"))}</h3>
      <div class="sub">${esc(sub)}${o.mini && S.smart[d.id]?.available ? ` <span class="hdot ${S.smart[d.id].level}" title="${HEALTH[S.smart[d.id].level]}"></span>${S.health[d.id]?.trends.some((t) => t.severity === "error" || t.code === "TREND_REALLOCATED" || t.code === "TREND_SPARE") ?` <b style="color:var(--warn)">${L("en dégradation", "degrading")}</b>` : ""}` : ""}</div>
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
  if (S.step === "result") return `<div class="page">${flight()}${outcomeHtml(S.report, S.runError)}</div>`;
  if (S.step === "review") return viewReview();
  const isSrc = S.step === "source";
  const s = src();
  const head = isSrc
    ? [L("Quel disque voulez-vous copier ?", "Which disk do you want to copy?"), L("Choisissez le disque source. Il ne sera jamais modifié : Chud Clone ne fait que le lire.", "Choose the source disk. It is never modified: Chud Clone only reads it.")]
    : [L("Sur quel disque écrire la copie ?", "Which disk should receive the copy?"), L("Choisissez le disque de destination. Tout son contenu actuel sera effacé.", "Choose the destination disk. All its current content will be erased.")];
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
        return diskCard(d, { act: "pick", selected: d.id === S.dstId, dest: true, tag: d.id === S.dstId ? "Destination" : "", chips, note, locked });
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
  const chosen = isSrc ? s : dst();
  return `<div class="page">${flight()}
    <div class="page-head"><div><h1>${head[0]}</h1><p>${head[1]}</p></div><button class="btn small ghost" data-act="refresh">↻ ${L("Actualiser", "Refresh")}</button></div>
    ${limited}${cards}
    <div class="footer-bar">
      ${isSrc ? "" : `<button class="btn" data-act="back">← ${L("Retour", "Back")}</button>`}
      <span class="muted">${chosen ? `${isSrc ? "Source" : "Destination"}${L(" :", ":")} <b style="color:var(--text)">${esc(chosen.model)}</b> · ${size(chosen.size)}` : L("Aucun disque sélectionné", "No disk selected")}</span>
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
    <div class="opt"><div><label>${L("Agrandir la partition Windows", "Grow the Windows partition")}</label><small>${L("Sur un disque plus grand, étend le volume NTFS principal sur l'espace ajouté, après la copie et sa vérification. Windows le contrôlera une fois (chkdsk) au premier démarrage.", "On a larger disk, extends the main NTFS volume over the added space, after the copy and its verification. Windows will check it once (chkdsk) on first boot.")}</small></div><label class="toggle"><input type="checkbox" data-opt="grow_main" ${o.grow_main !== false ? "checked" : ""}><span>${L("Activé", "Enabled")}</span></label></div>
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
    const ready = !p.blocked;
    const word = confirmWord();
    const good = S.confirmText.trim().toUpperCase() === word;
    // Un problème sérieux (disque source dégradé…) remonte en haut de page.
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
        ${plan.partitions.map((x) => `<tr><td>${x.number}</td><td>${esc(x.type_label)}${x.fs_label ? ` <span class="dim">${L(`« ${esc(x.fs_label)} »`, `“${esc(x.fs_label)}”`)}</span>` : ""}</td><td>${esc(FS[x.fs] || x.fs)}</td><td class="num">${size(x.src_len)}</td><td class="num">${x.used_bytes != null ? size(x.used_bytes) : "—"}</td><td>${METHOD[x.method]}${x.method === "raw" && x.method_reason ? `<div class="dim">${esc(x.method_reason)}</div>` : ""}</td><td>${x.moved ? L("déplacée", "moved") : L("inchangée", "unchanged")}${x.dst_len > x.src_len ? `<div class="grow-tag">${L("agrandie à", "grown to")} ${size(x.dst_len)}</div>` : ""}</td></tr>`).join("")}
        </table>
      </div>
      <div class="cols" style="margin-top:14px">
        <div class="panel" style="margin:0"><h2>${L("Santé des disques", "Disk health")}</h2><div style="display:grid;gap:10px">${smartLine("Source", p.smart_source)}${smartLine("Destination", p.smart_destination)}</div><p class="dim" style="margin-top:10px">${L("Indicateurs fournis par les disques eux-mêmes : une estimation, pas une certitude.", "Indicators reported by the disks themselves: an estimate, not a certainty.")}</p></div>
        <div class="panel" style="margin:0"><h2>${L("Démarrage de la source", "Source boot")}</h2>${p.boot.checks.length ? `<div class="checks">${p.boot.checks.map(checkRow).join("")}</div>` : `<p class="muted">${L("Aucun élément de démarrage : disque de données.", "No boot elements: data disk.")}</p>`}</div>
      </div>
      ${urgent ? "" : attention}
      ${optionsHtml()}
      ${ready ? `<div class="danger-zone" style="margin-top:14px"><h2 style="font-size:12px;letter-spacing:.09em;text-transform:uppercase;margin-bottom:8px">${L("Opération irréversible", "Irreversible operation")}</h2>
        <p class="big">${L(`Toutes les données de « ${esc(p.destination.model)} » (${size(p.destination.size)}) seront définitivement effacées.`, `All data on “${esc(p.destination.model)}” (${size(p.destination.size)}) will be permanently erased.`)}</p>
        <p class="muted" style="margin-top:4px">${L("Une fois le clonage lancé, l'ancien contenu de la destination n'est plus récupérable, même si vous annulez en cours de route. La source, elle, n'est jamais modifiée.", "Once cloning has started, the destination's previous content cannot be recovered, even if you cancel midway. The source is never modified.")}</p>
        <div class="row wrap" style="margin-top:14px"><span>${L("Pour confirmer, saisissez", "To confirm, type")} <b class="mono">${word}</b>${L(" :", ":")}</span><input type="text" id="confirm" class="${good ? "good" : ""}" value="${esc(S.confirmText)}" autocomplete="off" spellcheck="false" maxlength="12"><span class="spacer"></span><button class="btn danger" data-act="start" ${good ? "" : "disabled"}>${L("Lancer le clonage", "Start cloning")}</button></div></div>` : ""}`;
  } else body = "";
  return `<div class="page">${flight()}
    <div class="page-head"><div><h1>${L("Vérifiez avant de lancer", "Review before starting")}</h1><p>${s && d ? `${esc(s.model)} → ${esc(d.model)}` : ""}</p></div></div>
    ${body}
    <div class="footer-bar"><button class="btn" data-act="back">← ${L("Retour", "Back")}</button><span class="spacer"></span>${S.prep && !S.prepBusy ? `<button class="btn small ghost" data-act="reprepare">↻ ${L("Refaire l'analyse", "Analyse again")}</button>` : ""}</div></div>`;
}
const FS = { ntfs: "NTFS", fat12: "FAT12", fat16: "FAT16", fat32: "FAT32", exfat: "exFAT", bitlocker: "BitLocker", refs: "ReFS", ext: "ext2/3/4", xfs: "XFS", btrfs: "Btrfs", linux_swap: "Swap", luks: "LUKS", blank: "—", unknown: "—" };
function checkRow(c) {
  const m = { present: ["y", "✓"], missing: ["n", "×"], unknown: ["u", "?"] }[c.state];
  return `<div class="check ${m[0]}"><span class="s">${m[1]}</span><span>${esc(c.label)}${c.state === "unknown" ? ` <span class="dim">— ${L("non vérifiable", "not verifiable")}</span>` : c.state === "missing" ? ` <span class="dim">— ${L("absent", "missing")}${c.required ? "" : L(" (facultatif)", " (optional)")}</span>` : ""}</span></div>`;
}

// ------------------------------------------------------ Vue : clonage en cours
function viewRun() {
  const s = S.run.source, d = S.run.destination;
  return `<div class="page">${flight()}
    <div class="scene"><canvas id="beam"></canvas>
      <div class="body src"><div class="planet">${Cosmos.planet(s)}</div><div class="lbl">Source</div><div class="name" title="${esc(s.model)}">${esc(s.model)}</div></div>
      <div class="body dst"><div class="planet" id="dst-planet">${Cosmos.planet(d)}</div><div class="lbl">Destination</div><div class="name" title="${esc(d.model)}">${esc(d.model)}</div></div>
      <div class="ring"><svg viewBox="0 0 100 100"><defs><linearGradient id="ringflow" x1="0" x2="1"><stop offset="0" stop-color="#4fe3ff"/><stop offset=".55" stop-color="#3a7bff"/><stop offset="1" stop-color="#9a6bff"/></linearGradient></defs><circle class="track" cx="50" cy="50" r="44"/><circle class="bar" id="ring-bar" cx="50" cy="50" r="44" stroke-dasharray="276.46" stroke-dashoffset="276.46"/></svg>
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
    <div class="footer-bar"><span class="muted">${L("Ne débranchez aucun disque et ne mettez pas l'ordinateur en veille pendant l'opération.", "Do not unplug any disk or put the computer to sleep during the operation.")}</span><span class="spacer"></span><button class="btn" data-act="cancel" id="btn-cancel">${L("Annuler le clonage…", "Cancel cloning…")}</button></div></div>`;
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
  if (beam) {
    const k = p?.speed_bps ? Math.min(1, Math.max(0.12, Math.log10(p.speed_bps / 1e6) / 3)) : 0.25;
    beam.set({ preparing: "prepare", copying: "copy", recovering: "recover", writing_table: "table", verifying: "verify", growing: "table", checking: "check", done: "check" }[phase], k, r.err.read > 0);
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

// ------------------------------------------------------------ Vue : résultat
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
  // Clone allé au bout mais avec des secteurs remplacés par des zéros : utilisable, abîmé.
  const zeroed = o.table_written && o.bad_ranges.some((b) => b.outcome === "zero_filled");
  const next = [];
  if (zeroed) { const z = num(o.bad_ranges.reduce((a, b) => a + b.sectors, 0)); next.push(L(`Le clone est complet à l'exception de ${z} secteur(s) remplacé(s) par des zéros (liste ci-dessous). Relancer le clonage ne les récupérera probablement pas : le disque source est endommagé, évitez de le solliciter davantage.`, `The clone is complete except for ${z} sector(s) replaced with zeros (list below). Cloning again will probably not recover them: the source disk is damaged, avoid stressing it further.`)); }
  if ((good || zeroed) && physical) {
    next.push(L("Éteignez l'ordinateur, puis débranchez le disque source (ou mettez-le de côté). Tant que les deux disques sont branchés ensemble, Windows en garde un hors ligne car ils portent les mêmes identifiants.", "Shut the computer down, then unplug the source disk (or set it aside). While both disks are connected together, Windows keeps one of them offline because they carry the same identifiers."));
    next.push(L("Branchez le clone à la place de l'ancien disque et démarrez. Si l'ordinateur ne trouve pas le système, choisissez le nouveau disque dans le menu de démarrage du BIOS/UEFI.", "Connect the clone in place of the old disk and start the computer. If it does not find the system, pick the new disk in the BIOS/UEFI boot menu."));
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

function badge(kind) {
  const col = { ok: "#54f0b6", warn: "#ffc56b", incomplete: "#ff8a5c", failed: "#ff4f82" }[kind];
  const twin = kind === "ok" || kind === "warn";
  const mark = kind === "ok" ? `<path d="M36 51l9 9 19-21" fill="none" stroke="#04101c" stroke-width="6" stroke-linecap="round" stroke-linejoin="round"/>` : kind === "warn" ? `<path d="M50 34v20M50 64v2" stroke="#1c1204" stroke-width="6.5" stroke-linecap="round"/>` : kind === "incomplete" ? `<path d="M36 50h28" stroke="#1c0a04" stroke-width="6.5" stroke-linecap="round"/>` : `<path d="M38 38l24 24M62 38L38 62" stroke="#1c0410" stroke-width="6.5" stroke-linecap="round"/>`;
  return `<svg viewBox="0 0 100 100"><circle cx="50" cy="50" r="46" fill="${col}" opacity=".12"/><circle class="${twin ? "" : "twinkle"}" cx="50" cy="50" r="46" fill="none" stroke="${col}" stroke-opacity=".5" stroke-dasharray="${twin ? "none" : "4 7"}"/><circle cx="50" cy="50" r="30" fill="${col}"/>${mark}</svg>`;
}

// ------------------------------------------------------ Vue : disques et santé
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
  return `<div class="page"><div class="page-head"><div><h1>${L("Disques et santé", "Disks & health")}</h1><p>${L("Identité, partitions et indicateurs de santé. Tout ici est en lecture seule.", "Identity, partitions and health indicators. Everything here is read-only.")}</p></div><button class="btn small ghost" data-act="refresh">↻ ${L("Actualiser", "Refresh")}</button></div>
    <div class="split"><div class="disk-list">${S.disks.map((d) => diskCard(d, { mini: true, act: "disk-sel", selected: d.id === sel.id })).join("")}</div>
    <div>
      <div class="panel"><div class="row" style="gap:16px;margin-bottom:14px"><div style="width:74px;height:74px;flex:none">${Cosmos.planet(sel)}</div><div style="min-width:0"><h3 style="font-size:18px;font-weight:650">${esc(sel.model || L("Disque sans nom", "Unnamed disk"))}</h3><div class="chips">${diskChips(sel)}</div></div></div>${identity(sel)}
        ${sel.notes.length ? `<div style="margin-top:10px">${sel.notes.map((n) => `<div class="dim">· ${esc(n)}</div>`).join("")}</div>` : ""}</div>
      <div class="panel"><h2>Partitions</h2>${strip(sel.partitions, sel.size, false)}<div style="margin-top:12px">${parts}</div></div>
      <div class="panel"><h2>${L("État de santé", "Health")}</h2>${smartHtml}</div>
      <div class="panel"><h2>${L("Diagnostic des partitions et du démarrage", "Partition and boot diagnostics")}</h2>${insHtml}</div>
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

// Mise à jour du seul panneau de scan (pas de rendu complet toutes les 200 ms).
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
    // Léger décalage pseudo-aléatoire et stable : un champ d'étoiles plutôt qu'un quadrillage.
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

// Évolution de la santé : température (courbe) et compteurs de défauts (paliers).
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

// ------------------------------------------------------------ Vue : réglages
function viewSettings() {
  const e = S.env || {};
  return `<div class="page"><div class="page-head"><div><h1>${L("Réglages", "Settings")}</h1></div></div>
    <div class="panel"><h2>${L("Affichage", "Display")}</h2>
      <div class="opt"><div><label>${L("Langue", "Language")}</label><small>${L("Interface, diagnostics et nouveaux rapports.", "Interface, diagnostics and new reports.")}</small></div>
      <select data-ui="lang"><option value="fr" ${LANG === "fr" ? "selected" : ""}>Français</option><option value="en" ${LANG === "en" ? "selected" : ""}>English</option></select></div>
      <div class="opt"><div><label>Animations</label><small>${L("Le ciel étoilé, le faisceau de transfert et les transitions. « Réduites » fige le fond.", "The starry sky, the transfer beam and the transitions. \"Reduced\" freezes the background.")}</small></div>
      <select data-ui="motion"><option value="full" ${S.ui.motion === "full" ? "selected" : ""}>${L("Complètes", "Full")}</option><option value="reduced" ${S.ui.motion === "reduced" ? "selected" : ""}>${L("Réduites", "Reduced")}</option><option value="off" ${S.ui.motion === "off" ? "selected" : ""}>${L("Aucune", "None")}</option></select></div></div>
    <div class="panel"><h2>${L("Avancé", "Advanced")}</h2>
      <div class="finding warning" style="margin-bottom:12px"><div class="ico">!</div><div><b>${L("Écriture sur disque physique : pas encore validée sur du matériel", "Writing to a physical disk: not yet validated on hardware")}</b><p>${L(`Le moteur de Chud Clone ${esc(e.version || "")} a été testé sur des images disque (tables GPT/MBR, NTFS, FAT, secteurs illisibles, pannes simulées) et vérifié par des outils indépendants. L'écriture sur un vrai disque sous Windows, elle, n'a pas encore été éprouvée. Tant que ce n'est pas fait, elle reste désactivée par défaut. Le clonage vers un fichier image ou un disque virtuel (VHDX) fonctionne sans ce réglage.`, `The Chud Clone ${esc(e.version || "")} engine has been tested on disk images (GPT/MBR tables, NTFS, FAT, unreadable sectors, simulated failures) and checked by independent tools. Writing to a real disk under Windows has not been proven yet. Until it is, it stays disabled by default. Cloning to an image file or a virtual disk (VHDX) works without this setting.`)}</p></div></div>
      <label class="toggle"><input type="checkbox" data-opt="allow_unvalidated" ${S.opts.allow_unvalidated ? "checked" : ""} ${e.physical_write_validated ? "disabled" : ""}><span>${L("Autoriser l'écriture sur un disque physique réel, en connaissance de cause", "Allow writing to a real physical disk, knowingly")}</span></label></div>
    <div class="panel"><h2>${L("À propos", "About")}</h2>${kv([["Version", `${esc(e.name)} ${esc(e.version)}`], ["Mode", e.portable ? L("Portable (données à côté du programme)", "Portable (data next to the program)") : L("Installé", "Installed")], [L("Droits", "Rights"), e.elevated ? L("Administrateur", "Administrator") : L("Limités (lecture des propriétés uniquement)", "Limited (properties only)")], [L("Données", "Data"), `<code>${esc(e.data_dir)}</code>`], [L("Rapports", "Reports"), `<code>${esc(e.reports_dir)}</code>`], [L("Confidentialité", "Privacy"), L("Aucune télémétrie, aucune connexion réseau.", "No telemetry, no network connection.")]])}</div></div>`;
}

// ------------------------------------------------------------------- Rendu
// Transition « glitch » de la nouvelle page : la page apparaît découpée en bandes qui se
// décalent horizontalement avec une aberration chromatique, le titre se dédouble, et des
// bandes lumineuses traversent la zone. Tout est retiré à la fin : un rendu ultérieur du
// même écran ne rejoue rien.
let glitchTimer = 0;
let glitchStart = -1;
const GLITCH_MS = 650;
// Dédouble le titre ; `elapsed` reprend l'animation là où elle en était si la page a été
// redessinée pendant la transition.
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
    const colors = ["rgba(79,227,255,.35)", "rgba(255,79,130,.3)", "rgba(154,107,255,.3)", "rgba(232,236,255,.18)"];
    for (let i = 0; i < 7; i++) {
      const b = document.createElement("i");
      const w = 30 + Math.random() * 70;
      Object.assign(b.style, {
        top: Math.random() * 92 + "%", height: 2 + Math.random() * 16 + "px", width: w + "%", left: Math.random() * (100 - w) + "%",
        background: colors[i % colors.length], animationDelay: Math.round(Math.random() * 120) + "ms",
      });
      b.style.setProperty("--dx", (Math.random() < 0.5 ? -1 : 1) * (10 + Math.random() * 40) + "px");
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
  view.innerHTML = { clone: viewClone, disks: viewDisks, reports: viewReports, settings: viewSettings }[S.view]();
  view.dataset.screen = screen;
  view.scrollTop = sameScreen ? scroll : 0;
  // Transition seulement quand l'écran change : les rafraîchissements (chargement, SMART,
  // progression) remplacent le contenu sans rien rejouer.
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
    const pre = $("#log");
    for (const e of S.run.logs) {
      const line = document.createElement("div");
      line.innerHTML = `<span class="t">${esc(e.time)}</span> <span class="${e.level === "warning" ? "w" : e.level === "error" ? "e" : ""}">${esc(e.message)}</span>`;
      pre.appendChild(line);
    }
    updateRun();
  }
  if (S.view === "clone" && S.step === "review") $("#confirm")?.addEventListener("input", (ev) => {
    S.confirmText = ev.target.value;
    const ok = S.confirmText.trim().toUpperCase() === confirmWord();
    ev.target.classList.toggle("good", ok);
    const b = $('[data-act="start"]');
    if (b) b.disabled = !ok;
  });
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

function params() {
  const o = S.opts;
  return {
    source_id: S.srcId, destination_id: S.dstId, mode: o.mode, optimize_layout: !!o.optimize_layout, grow_main: o.grow_main !== false, verify: o.verify, unreadable: o.unreadable,
    read_retries: Number(o.read_retries), allow_unvalidated_physical_write: !!o.allow_unvalidated,
    source_fingerprint: S.prep?.source.fingerprint ?? null, destination_fingerprint: S.prep?.destination.fingerprint ?? null,
  };
}

async function prepare() {
  S.prep = null; S.prepError = null; S.prepBusy = true; S.confirmText = "";
  S.prepLang = LANG;
  render();
  await langReady;
  try {
    S.prep = await api.invoke("prepare_clone", { params: params() });
    S.smart[S.prep.source.id] = S.prep.smart_source;
    S.smart[S.prep.destination.id] = S.prep.smart_destination;
  } catch (e) { S.prepError = typeof e === "string" ? e : e?.message || String(e); }
  S.prepBusy = false;
  if (S.view === "clone" && S.step === "review") render();
}

// ----------------------------------------------------------------- Actions
const actions = {
  pick(el) {
    if (S.step === "source") { S.srcId = el.dataset.id; if (S.dstId === S.srcId) S.dstId = null; }
    else S.dstId = el.dataset.id;
    render();
  },
  next() {
    if (S.step === "source" && S.srcId) S.step = "destination";
    else if (S.step === "destination" && S.dstId) { S.step = "review"; prepare(); return; }
    render();
  },
  back() {
    S.step = S.step === "review" ? "destination" : "source";
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
      if (S.step === "source") S.srcId = info.id; else if (info.id !== S.srcId) S.dstId = info.id;
      render();
    } catch (e) { fail(e); }
  },
  async "new-image"() {
    const s = src();
    if (!s) return;
    try {
      const path = await api.save({ title: L("Enregistrer l'image de destination", "Save the destination image"), defaultPath: "clone.img", filters: [{ name: L("Image disque", "Disk image"), extensions: ["img"] }] });
      if (!path) return;
      const info = await api.invoke("add_new_image", { path, size: s.size });
      await loadDisks(true);
      S.dstId = info.id;
      render();
    } catch (e) { fail(e); }
  },
  async start() {
    if (!S.prep || S.prep.blocked) return;
    const p = params();
    S.run = { source: S.prep.source, destination: S.prep.destination, progress: null, logs: [], started: Date.now(), err: { read: 0, bad: 0, rec: 0, write: 0 } };
    S.report = null; S.runError = null; S.runBlocked = null;
    try {
      await api.invoke("start_clone", { params: p, confirmation: S.confirmText.trim().toUpperCase() });
      S.step = "run";
      render();
    } catch (e) { S.run = null; fail(e); }
  },
  cancel() {
    modal(`<h2>${L("Annuler le clonage ?", "Cancel cloning?")}</h2><p>${L("La copie s'arrêtera immédiatement. <b>L'ancien contenu de la destination est déjà perdu</b> et ne sera pas restauré : la destination restera sans table de partitions, inutilisable jusqu'à un nouveau clonage complet.", "The copy will stop immediately. <b>The destination's previous content is already lost</b> and will not be restored: the destination will stay without a partition table, unusable until a new complete clone.")}</p><p class="muted">${L("La source n'est pas modifiée.", "The source is not modified.")}</p>
      <div class="actions"><button class="btn primary" data-act="modal-close">${L("Continuer le clonage", "Keep cloning")}</button><button class="btn danger" data-act="cancel-yes">${L("Annuler le clonage", "Cancel cloning")}</button></div>`);
  },
  "cancel-yes"() { closeModal(); api.invoke("cancel_clone"); const b = $("#btn-cancel"); if (b) { b.disabled = true; b.textContent = L("Annulation en cours…", "Cancelling…"); } },
  "modal-close"() { closeModal(); },
  "unreadable-stop"() { closeModal(); api.invoke("answer_unreadable", { zeroFill: false }); },
  "unreadable-zero"() { closeModal(); api.invoke("answer_unreadable", { zeroFill: true }); },
  restart() { S.step = "source"; S.prep = null; S.report = null; S.runError = null; S.run = null; S.confirmText = ""; S.dstId = null; loadDisks(); },
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
  async smart(el) {
    await readHealth(el.dataset.id);
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
    try {
      S.scan = { id, running: true, cells: new Array(Math.min(2400, Math.max(1, Math.ceil(d.size / 1048576)))).fill(255), progress: null, result: null, error: null, cursor: 0 };
      await api.invoke("start_scan", { id });
    } catch (e) { S.scan = null; fail(e); }
    render();
  },
  "scan-cancel"() { if (S.scan) { S.scan.cancelling = true; api.invoke("cancel_scan"); render(); } },
  async "report-open"(el) {
    try { S.reportOpen = await api.invoke("get_report", { id: el.dataset.id }); render(); } catch (e) { fail(e); }
  },
  "report-close"() { S.reportOpen = null; render(); },
  async elevate() {
    try { await api.invoke("relaunch_elevated"); } catch (e) { fail(e); }
  },
};

// Lecture de santé (commande en lecture seule) + historique : chaque relevé est conservé
// pour signaler ensuite ce qui s'aggrave.
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

// À la première visite de « Disques et santé », relève la santé de tous les disques.
async function surveyHealth() {
  if (S.surveyed) return;
  S.surveyed = true;
  for (const d of S.disks.filter((x) => x.kind === "physical")) {
    if (!S.smart[d.id]) await readHealth(d.id, true);
  }
}

async function goto(view) {
  if (S.view === "clone" && S.step === "run" && view !== "clone") { toast(L("Un clonage est en cours : restez sur cet écran pour le suivre.", "A clone is running: stay on this screen to follow it.")); return; }
  S.view = view;
  // Analyse affichée dans une autre langue : elle est refaite.
  if (view === "clone" && S.step === "review" && !S.prepBusy && S.prepLang !== LANG) return void prepare();
  if (view === "reports") { S.reportOpen = null; S.reports = null; render(); try { S.reports = await api.invoke("list_reports"); } catch (e) { S.reports = []; fail(e); } }
  render();
  if (view === "disks" && !S.loadingDisks) surveyHealth();
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
    // Une option qui change le plan impose de refaire l'analyse affichée.
    if (S.view === "clone" && S.step === "review" && ["mode", "optimize_layout", "grow_main"].includes(t.dataset.opt)) prepare();
  }
  if (t.dataset.ui) {
    S.ui[t.dataset.ui] = t.value;
    store.set("ui", S.ui);
    if (t.dataset.ui === "lang") applyLang(t.value, true);
    else Cosmos.setQuality(S.ui.motion);
  }
});
document.addEventListener("keydown", (ev) => { if (ev.key === "Escape" && $("#modal-root .overlay") && !$('[data-act="unreadable-stop"]')) closeModal(); });

// Mot à saisir pour confirmer l'effacement (le moteur accepte les deux).
const confirmWord = () => L("EFFACER", "ERASE");

// Change de langue : libellés, textes fixes de la page et messages du moteur. Ce que le
// moteur a déjà produit (analyses, santé) est relu pour être affiché dans la nouvelle langue.
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

// -------------------------------------------------------------- Démarrage
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
    const { report, error, blocked } = e.payload;
    S.report = report; S.runError = error; S.runBlocked = blocked;
    const st = report ? STATUS[report.outcome.status][0] : "failed";
    // Laisse voir l'état final du faisceau avant d'afficher le rapport.
    if (beam) beam.set({ ok: "ok", warn: "warn", incomplete: "incomplete", failed: "fail" }[st], 0.3, false);
    setTimeout(() => { S.step = "result"; S.run = null; render(); loadDisks(true); }, document.documentElement.classList.contains("no-motion") ? 0 : 1600);
  });
  await api.listen("scan-progress", (e) => {
    const sc = S.scan, p = e.payload;
    if (!sc || !sc.running) return;
    sc.progress = p;
    for (const [i, c] of p.cells) { sc.cells[i] = c; sc.cursor = i; }
    refreshScanPanel();
  });
  await api.listen("scan-done", (e) => {
    const { id, result, error, saved } = e.payload;
    if (!S.scan || S.scan.id !== id) S.scan = { id };
    Object.assign(S.scan, { running: false, cancelling: false, result, error, saved, cells: result ? result.cells : S.scan.cells || [] });
    if (result?.bad_sectors) toast(L(`Scan terminé : ${num(result.bad_sectors)} secteur(s) illisible(s).`, `Scan finished: ${num(result.bad_sectors)} unreadable sector(s).`), true);
    else if (result) toast(L("Scan de surface terminé.", "Surface scan finished."));
    if (S.view === "disks") render();
  });
  await api.listen("close-blocked", () => toast(L("Un clonage est en cours. Annulez-le d'abord si vous voulez vraiment fermer.", "A clone is running. Cancel it first if you really want to close."), true));
  setInterval(() => { if (S.step === "run" && S.run) updateRun(); }, 1000);
  try { S.env = await api.invoke("env_info"); } catch (e) { fail(e); }
  // Sans choix enregistré : langue de Windows.
  if (!S.ui.lang && S.env?.system_lang) applyLang(S.env.system_lang);
  render();
  await loadDisks();
}
boot();
