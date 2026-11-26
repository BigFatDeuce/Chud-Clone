// UI bench: replaces the Tauri API with a simulated engine.
// The structures (plan, report) come from real engine output (mock-data.js);
// only sizes and disks are adapted to look like a real PC.
// Nothing here touches a disk: this file is never shipped with the application.
//
// URL parameters: ?s=ok|bad|fail|blocked  ?admin=0  ?fast=1  ?view=disks|reports|settings  ?lang=en
"use strict";
(function () {
  const Q = new URLSearchParams(location.search);
  const SCEN = Q.get("s") || "ok";
  const ADMIN = Q.get("admin") !== "0";
  const FAST = Q.get("fast") === "1" ? 4 : 1;
  // Screenshots (?demo=…): speeds and durations of a real SATA SSD.
  const DEMO = Q.has("demo");
  // Language of the simulated engine (like the real one: set by set_language).
  let MLANG = "fr";
  const T = (fr, en) => (MLANG === "en" ? en : fr);
  const D = () => (MLANG === "en" && window.MOCK_DATA_EN) || window.MOCK_DATA;
  const GB = (b) => { const x = (b / GiB).toFixed(1); return MLANG === "en" ? x + " GB" : x.replace(".", ",") + " Go"; };
  const MiB = 1048576, GiB = 1024 * MiB;
  const clone = (o) => JSON.parse(JSON.stringify(o));
  const wait = (ms) => new Promise((r) => setTimeout(r, ms / FAST));
  const fp = (s) => Array.from(s).reduce((h, c) => (h * 31 + c.charCodeAt(0)) >>> 0, 7).toString(16).padStart(16, "0");

  // --------------------------------------------------------------- Disques fictifs
  const part = (number, offset, size, role, type_label, fs, label, used, mounts) => ({ number, offset, size, role, type_label, name: null, fs, label, used_bytes: used, mounts: mounts || [], active: false, volume_path: null });
  function winParts(total, used, mount) {
    const rec = 750 * MiB, end = total - MiB - rec;
    return [
      part(1, MiB, 100 * MiB, "esp", T("Partition système EFI", "EFI system partition"), "FAT32", "SYSTEM", 31 * MiB),
      part(2, 101 * MiB, 16 * MiB, "msr", T("Réservée Microsoft (MSR)", "Microsoft reserved (MSR)"), null, null, null),
      part(3, 117 * MiB, end - 117 * MiB, "basic_data", T("Données de base", "Basic data"), "NTFS", "Windows", used, mount ? [mount] : []),
      part(4, end, rec, "recovery", T("Récupération Windows", "Windows recovery"), "NTFS", "Recovery", 532 * MiB),
    ];
  }
  function mk(o) {
    const d = Object.assign({ kind: "physical", vendor: "", firmware: "", sector_size: 512, physical_sector_size: 4096, removable: false, read_only: false, offline: false, is_system: false, hosts_app: false, table: "gpt", partitions: [], used_bytes: null, deep: true, notes: [] }, o);
    d.id = d.id || `disk:${d.number}`;
    d.path = d.path || `\\\\.\\PhysicalDrive${d.number}`;
    d.fingerprint = fp(d.id + d.serial + d.size);
    if (d.used_bytes == null && d.partitions.length) d.used_bytes = d.partitions.reduce((a, p) => a + (p.used_bytes || 0), 0) || null;
    return d;
  }
  const sys = 1000204886016, evo = 500107862016, wd = 2000398934016, mx = 1000204886016;
  let disks;
  const images = [];
  const buildDisks = () => [
    mk({ number: 0, model: "Samsung SSD 980 PRO 1TB", serial: "S5GXNX0T512345", bus: "NVMe", media: "ssd", size: sys, is_system: true, hosts_app: true, partitions: winParts(sys, 412 * GiB, "C:\\"), notes: [T("Contient le Windows en cours d'exécution.", "Holds the running Windows."), T("Contient le dossier de Chud Clone.", "Holds the Chud Clone folder.")] }),
    mk({ number: 1, model: "Samsung SSD 860 EVO 500GB", serial: "S3Z1NB0K412873W", bus: "SATA", media: "ssd", size: evo, partitions: winParts(evo, 187 * GiB, "E:\\") }),
    mk({ number: 2, model: "WDC WD20EZRZ-00Z5HB0", serial: "WD-WCC4M2KJ7XL1", bus: "SATA", media: "hdd", size: wd, partitions: [part(1, MiB, wd - 2 * MiB, "basic_data", T("Données de base", "Basic data"), "NTFS", "Archives", 1.21 * 1024 * GiB, ["D:\\"])] }),
    mk({ number: 3, model: "CT1000MX500SSD1", serial: "2203E5F1A2B4", bus: "SATA", media: "ssd", size: mx, table: "none" }),
    mk({ number: 4, model: "SanDisk Ultra USB 3.0", serial: "4C530001230712108543", bus: "USB", media: "unknown", size: 30752636928, removable: true, table: "mbr", partitions: [Object.assign(part(1, MiB, 30751588352, "basic_data", "FAT32 (LBA)", "FAT32", "CLE", 4.3 * GiB, ["F:\\"]), { active: true })] }),
  ];
  // Disks rebuilt on every language change (engine labels), added images kept.
  function setDisks() {
    disks = buildDisks().concat(images);
    if (SCEN === "blocked") disks[1].is_system = true;
    // Destination smaller than the source's partitions: the Windows partition is shrunk.
    if (SCEN === "shrink") disks[3].size = 256060514304;
  }
  setDisks();

  const smartOf = (d) => {
    if (d.kind === "image") return { available: false, protocol: "", level: "unknown", drive_reports_failure: null, temperature_c: null, power_on_hours: null, power_cycles: null, wear_percent_used: null, attributes: [], nvme: null, findings: [], unavailable_reason: T("fichier image : pas de compteurs de santé", "image file: no health counters") };
    if (d.bus === "USB") return { available: false, protocol: "", level: "unknown", drive_reports_failure: null, temperature_c: null, power_on_hours: null, power_cycles: null, wear_percent_used: null, attributes: [], nvme: null, findings: [], unavailable_reason: T("le boîtier USB ne transmet pas les commandes SMART", "the USB enclosure does not pass SMART commands through") };
    const bad = SCEN === "bad" && d.number === 1;
    const attrs = [[5, T("Secteurs réalloués", "Reallocated sectors"), 100, 100, 10, bad ? 24 : 0, true], [9, T("Heures de fonctionnement", "Power-on hours"), 97, 97, 0, 13026, false], [12, T("Mises sous tension", "Power cycles"), 99, 99, 0, 1450, false], [177, T("Usure (cycles d'effacement)", "Wear leveling count"), 93, 93, 0, 71, true], [187, T("Erreurs non corrigibles signalées", "Reported uncorrectable errors"), 100, 100, 0, bad ? 3 : 0, false], [190, T("Température du flux d'air", "Airflow temperature"), 66, 52, 0, 34, false], [197, T("Secteurs en attente de réallocation", "Pending sectors"), 100, 100, 0, bad ? 8 : 0, false], [199, T("Erreurs CRC (câble/liaison)", "CRC errors (cable/link)"), 100, 100, 0, 0, false]];
    if (d.bus === "NVMe") return { available: true, protocol: "NVMe", level: "good", drive_reports_failure: false, temperature_c: 45, power_on_hours: 13026, power_cycles: 2210, wear_percent_used: 1, attributes: [], nvme: { critical_warning: 0, available_spare: 100, spare_threshold: 10, percentage_used: 1, data_read_bytes: 41.2 * 1024 * GiB, data_written_bytes: 38.9 * 1024 * GiB, unsafe_shutdowns: 87, media_errors: 0, error_log_entries: 0 }, findings: [], unavailable_reason: null };
    return {
      available: true, protocol: "ATA", level: bad ? "bad" : d.media === "hdd" ? "watch" : "good", drive_reports_failure: false, temperature_c: d.media === "hdd" ? 39 : 31 + d.number, power_on_hours: d.media === "hdd" ? 31877 : 4100 * d.number + 812, power_cycles: 300 * d.number + 41, wear_percent_used: d.media === "ssd" ? 2 * d.number : null,
      attributes: attrs.map(([id, name, current, worst, threshold, raw, prefail]) => ({ id, name, current, worst, threshold, raw, prefail, below_threshold: false })),
      nvme: null,
      findings: bad
        ? [{ severity: "error", code: "SMART_PENDING", title: T("Secteurs en attente de réallocation : 8", "Sectors pending reallocation: 8"), detail: T("Le disque n'arrive plus à lire certains secteurs. Clonez-le au plus vite et n'insistez pas inutilement : chaque nouvelle lecture peut l'user davantage.", "The disk can no longer read some sectors. Clone it as soon as possible and do not insist needlessly: every new read can wear it further."), blocking: false, scope: null, lba: null },
           { severity: "warning", code: "SMART_REALLOCATED", title: T("Secteurs réalloués : 24", "Reallocated sectors: 24"), detail: T("Le disque a déjà remplacé des secteurs défectueux par des secteurs de réserve.", "The disk has already replaced faulty sectors with spare ones."), blocking: false, scope: null, lba: null }]
        : d.media === "hdd" ? [{ severity: "warning", code: "SMART_AGE", title: T("Disque ancien : 31 877 heures de fonctionnement", "Old disk: 31,877 power-on hours"), detail: T("Rien d'anormal n'est signalé, mais un disque dur de cet âge mérite une sauvegarde régulière.", "Nothing abnormal is reported, but a hard disk of this age deserves regular backups."), blocking: false, scope: null, lba: null }] : [],
      unavailable_reason: null,
    };
  };

  // ------------------------------------------------------------------- Plan
  function buildPrep(params) {
    const s = disks.find((d) => d.id === params.source_id), d = disks.find((x) => x.id === params.destination_id);
    const prep = clone(D().prep);
    prep.source = s; prep.destination = d;
    const tpl = D().prep.plan.partitions;
    const byRole = (r) => tpl.find((p) => p.role === r) || tpl[0];
    const parts = s.partitions.map((p) => {
      const t = clone(byRole(p.role));
      const msr = p.role === "msr";
      const raw = params.mode !== "smart";
      return Object.assign(t, { number: p.number, role: p.role, type_label: p.type_label, fs: (p.fs || "unknown").toLowerCase(), fs_label: p.label, used_bytes: p.used_bytes, src_start: p.offset, src_len: p.size, dst_start: p.offset, dst_len: p.size, method: msr ? "skip" : raw ? "raw" : "smart", method_reason: raw && !msr ? T("mode « secteur par secteur » choisi", "\"sector by sector\" mode chosen") : null, copy_bytes: msr ? 0 : raw ? p.size : Math.round(p.used_bytes * 1.01), moved: false });
    });
    let free = 0;
    const last = parts[parts.length - 1];
    if (d.size > s.size && params.optimize_layout && last && last.role === "recovery" && s.table === "gpt") {
      last.dst_start = Math.floor((d.size - MiB - last.src_len) / MiB) * MiB;
      last.moved = true;
      const main = parts.find((p) => p.role === "basic_data");
      free = last.dst_start - (main.dst_start + main.dst_len);
    } else if (d.size > s.size) free = d.size - s.size;
    // Automatic growth of the Windows (NTFS) partition onto the freed space.
    const mainP = parts.find((p) => p.role === "basic_data" && p.fs === "ntfs");
    let grow = null;
    if (free > 16 * MiB && mainP && params.grow_main !== false) {
      grow = { number: mainP.number, dst_start: mainP.dst_start, old_len: mainP.src_len, new_len: mainP.src_len + free };
      mainP.dst_len = grow.new_len;
    }
    // Destination smaller than the partitions: the Windows partition is shrunk (packed layout).
    let shrink = null;
    const need = parts.reduce((a, p) => a + p.src_len, 0) + 2 * MiB;
    if (d.size < need && mainP && params.grow_main !== false && params.mode === "smart") {
      const cut = Math.ceil((need - d.size) / MiB) * MiB;
      if (mainP.src_len - cut > mainP.used_bytes * 1.1) {
        shrink = { number: mainP.number, src_start: mainP.src_start, dst_start: mainP.dst_start, old_len: mainP.src_len, new_len: mainP.src_len - cut, relocated_bytes: Math.round(mainP.used_bytes * 0.18) };
        mainP.dst_len = shrink.new_len;
        let cur = MiB;
        for (const p of parts) { p.dst_start = cur; p.moved = p.dst_start !== p.src_start; cur += Math.ceil(p.dst_len / MiB) * MiB; }
      }
    }
    Object.assign(prep.plan, { partitions: parts, src_size: s.size, dst_size: d.size, total_copy_bytes: parts.reduce((a, p) => a + p.copy_bytes, 0), free_after_main: grow ? 0 : free, grow, shrink, mode: params.mode });
    const fsize = GB;
    prep.findings = clone(D().prep.findings).map((f) =>
      f.code === "LAYOUT_TRAILING_MOVED"
        ? Object.assign(f, { title: T("Partitions de fin placées en fin de disque", "Trailing partitions placed at the end of the disk"), detail: T(`Les partitions situées après la partition principale sont placées en fin de disque, pour que l'espace supplémentaire soit contigu à la partition ${mainP ? mainP.number : 3}.`, `The partitions after the main partition are placed at the end of the disk, so that the extra space is contiguous with partition ${mainP ? mainP.number : 3}.`) })
        : f);
    prep.findings = prep.findings.filter((f) => !["NTFS_GROW_PLANNED", "GROW_NOT_AUTOMATIC"].includes(f.code));
    if (!parts.some((p) => p.moved)) prep.findings = prep.findings.filter((f) => !["LAYOUT_TRAILING_MOVED", "RECOVERY_MOVED"].includes(f.code));
    if (shrink) prep.findings = prep.findings.filter((f) => f.code !== "LAYOUT_TRAILING_MOVED");
    prep.smart_source = smartOf(s); prep.smart_destination = smartOf(d);
    const F = (severity, code, title, detail, blocking) => ({ severity, code, title, detail, blocking: !!blocking, scope: null, lba: null });
    if (shrink) prep.findings.push(F("info", "NTFS_SHRINK_PLANNED", T(`Partition ${shrink.number} réduite pour tenir sur la destination`, `Partition ${shrink.number} shrunk to fit the destination`), T(`La partition ${shrink.number} passera de ${fsize(shrink.old_len)} à ${fsize(shrink.new_len)}. Pendant la copie, les données situées au-delà de la nouvelle fin (${fsize(shrink.relocated_bytes)}) sont recopiées plus bas dans l'espace libre et le système de fichiers NTFS est mis à jour sur la destination ; la source n'est pas modifiée. Le volume est ensuite marqué « à vérifier » : au premier démarrage du clone, Windows le contrôle une fois (chkdsk) avant l'ouverture de session. C'est normal et peut prendre quelques minutes.`, `Partition ${shrink.number} will go from ${fsize(shrink.old_len)} to ${fsize(shrink.new_len)}. During the copy, the data located beyond the new end (${fsize(shrink.relocated_bytes)}) is copied lower down into the free space and the NTFS file system is updated on the destination; the source is not modified. The volume is then flagged "needs checking": on the clone's first boot, Windows checks it once (chkdsk) before sign-in. This is normal and can take a few minutes.`)));
    else if (d.size < s.size) prep.findings.push(F("warning", "DEST_SMALLER", T("Destination plus petite que la source", "Destination smaller than the source"), T("Les partitions seront rapprochées ; la partition principale garde sa taille. Vérifiez qu'elle tient sur la destination.", "The partitions will be packed together; the main partition keeps its size. Check that it fits on the destination.")));
    if (grow) prep.findings.push(F("info", "NTFS_GROW_PLANNED", T(`Partition ${grow.number} agrandie automatiquement`, `Partition ${grow.number} grown automatically`), T(`La partition ${grow.number} passera de ${fsize(grow.old_len)} à ${fsize(grow.new_len)} (+${fsize(free)}). Après la copie et sa vérification, son système de fichiers NTFS est étendu sur place, sans déplacer aucune donnée (mêmes modifications que l'outil de référence ntfsresize). Le volume est ensuite marqué « à vérifier » : au premier démarrage du clone, Windows le contrôle une fois (chkdsk) avant l'ouverture de session. C'est normal et peut prendre quelques minutes.`, `Partition ${grow.number} will go from ${fsize(grow.old_len)} to ${fsize(grow.new_len)} (+${fsize(free)}). After the copy and its verification, its NTFS file system is extended in place, without moving any data (same changes as the reference tool ntfsresize). The volume is then marked "to be checked": on the clone's first boot, Windows checks it once (chkdsk) before sign-in. This is normal and can take a few minutes.`)));
    else if (free > 16 * MiB && mainP) prep.findings.push(F("info", "GROW_NOT_AUTOMATIC", T("Espace supplémentaire à récupérer", "Extra space to reclaim"), T(`${fsize(free)} non alloués se trouveront juste après la partition ${mainP.number}. Agrandissement automatique impossible : l'agrandissement automatique est désactivé dans les options. Une fois le clone en service, « Gestion des disques » → clic droit sur la partition → « Étendre le volume » permet de récupérer cet espace.`, `${fsize(free)} of unallocated space will sit right after partition ${mainP.number}. Automatic growth not possible: automatic growth is disabled in the options. Once the clone is in use, "Disk Management" → right-click the partition → "Extend Volume" reclaims this space.`)));
    if (d.partitions.length) prep.findings.push(F("warning", "DEST_HAS_DATA", T(`La destination contient ${d.partitions.length} partition(s)`, `The destination holds ${d.partitions.length} partition(s)`), T("Elles seront effacées définitivement.", "They will be permanently erased.")));
    if (s.bus === "USB" || d.bus === "USB") prep.findings.push(F("info", "USB_INVOLVED", T("Disque USB concerné", "USB disk involved"), T("Une connexion USB peut être plus lente et se couper si le câble bouge. Évitez les concentrateurs non alimentés.", "A USB connection can be slower and drop if the cable moves. Avoid unpowered hubs.")));
    prep.findings.push(F("info", "IDENTITY_PRESERVED", T("Le clone portera les mêmes identifiants que la source", "The clone will carry the same identifiers as the source"), T("Windows ne peut pas utiliser les deux disques en même temps : après le clonage, débranchez l'un des deux avant de redémarrer.", "Windows cannot use both disks at the same time: after cloning, unplug one of them before restarting.")));
    if (s.is_system) prep.findings.push(F("critical", "SOURCE_LIVE_SYSTEM", T("La source est le Windows en cours d'exécution", "The source is the running Windows"), T("Une copie faite pendant que Windows tourne ne serait pas cohérente (fichiers modifiés pendant la copie). Clonez ce disque depuis un autre Windows ou un environnement de secours.", "A copy made while Windows runs would not be consistent (files modified during the copy). Clone this disk from another Windows or a rescue environment."), true));
    if (d.is_system) prep.findings.push(F("critical", "DEST_IS_SYSTEM", T("La destination est le disque du Windows en cours", "The destination is the running Windows' disk"), T("Impossible de l'effacer.", "It cannot be erased."), true));
    if (prep.smart_source.level === "bad") prep.findings.push(...prep.smart_source.findings);
    const order = { critical: 0, error: 1, warning: 2, info: 3 };
    prep.findings.sort((a, b) => order[a.severity] - order[b.severity]);
    prep.blocked = prep.findings.some((f) => f.blocking);
    return prep;
  }

  // --------------------------------------------------------- Simulated clone
  const handlers = {};
  const emit = (ev, payload) => (handlers[ev] || []).forEach((h) => h({ payload }));
  let job = null;
  let scanCtl = null;
  const now = () => new Date().toLocaleTimeString(MLANG === "en" ? "en-GB" : "fr-FR");
  function log(level, message) {
    const e = { t_ms: Date.now() - job.t0, time: now(), level, message };
    job.log.push(e);
    emit("clone-log", e);
  }
  async function phase(name, total, secs, opts) {
    opts = opts || {};
    const steps = Math.max(1, Math.round(secs * 10));
    for (let i = 1; i <= steps; i++) {
      if (job.cancel) return false;
      if (opts.pauseAt != null && i === Math.round(steps * opts.pauseAt)) await opts.onPause();
      if (job.cancel) return false;
      const done = Math.round((total * i) / steps);
      const sp = DEMO ? 512e6 * (0.96 + Math.random() * 0.08) : total / secs * (0.85 + Math.random() * 0.3);
      const eta = DEMO ? Math.round((total - done) / 512e6) : Math.round((secs * (steps - i)) / steps * FAST);
      const p = { phase: name, done, total, elapsed_ms: 0, speed_bps: Math.round(opts.slow ? sp / 40 : sp), avg_bps: Math.round(DEMO ? 512e6 : total / secs), eta_secs: eta, read_errors: job.err.read, bad_bytes: job.err.bad, recovered_bytes: job.err.rec, write_errors: job.err.write, current_lba: Math.round((done / total) * (job.prep.plan.src_size / 512)) };
      emit("clone-progress", p);
      await wait(100);
    }
    return true;
  }

  async function runJob() {
    const plan = job.prep.plan, total = plan.total_copy_bytes;
    log("info", T(`Début du clonage — mode : ${{ smart: "Intelligent (blocs utilisés)", raw_partitions: "Partitions complètes", raw_disk: "Disque entier" }[plan.mode]} ; vérification : ${job.params.verify}`, `Clone started — mode: ${{ smart: "Smart (used blocks)", raw_partitions: "Whole partitions", raw_disk: "Whole disk" }[plan.mode]}; verification: ${job.params.verify}`));
    log("info", T("Verrouillage des volumes de la source : E:", "Locking the source volumes: E:"));
    log("info", T("Préparation de la destination", "Preparing the destination"));
    emit("clone-progress", { phase: "preparing", done: 0, total: 0, elapsed_ms: 0, speed_bps: 0, avg_bps: 0, eta_secs: null, read_errors: 0, bad_bytes: 0, recovered_bytes: 0, write_errors: 0, current_lba: 0 });
    await wait(1400);
    log("info", T("Anciennes tables de partitions de la destination effacées", "Old partition tables of the destination erased"));
    log("info", T("Copie des données", "Copying data"));
    let decision = "zero";
    let ok = await phase("copying", total, 14, SCEN === "bad" ? {
      pauseAt: 0.62, onPause: async () => {
        job.err.read += 3; job.err.bad += 64 * 1024;
        log("warning", T("Erreur de lecture à LBA 412 334 080 (secteur illisible) — zone isolée, relecture différée", "Read error at LBA 412,334,080 (unreadable sector) — area isolated, re-read deferred"));
        await wait(800);
        job.err.read += 2; job.err.bad += 8 * 512;
        log("warning", T("Erreur de lecture à LBA 698 112 512 (secteur illisible) — zone isolée, relecture différée", "Read error at LBA 698,112,512 (unreadable sector) — area isolated, re-read deferred"));
      },
    } : SCEN === "fail" ? { pauseAt: 0.45, onPause: async () => { job.fail = true; job.cancel = true; } } : null);
    if (ok && SCEN === "bad") {
      log("info", T("Nouvelle lecture des zones difficiles, secteur par secteur", "Re-reading difficult areas, sector by sector"));
      job.err.rec = 64 * 1024 - 3 * 512; job.err.bad = 3 * 512 + 8 * 512 - 4 * 512;
      ok = await phase("recovering", job.err.bad + job.err.rec, 3, { slow: true });
      if (ok) {
        log("warning", T("Secteurs restés illisibles : 7 (3,5 Ko) après 3 tentatives", "Sectors still unreadable: 7 (3.5 KB) after 3 attempts"));
        job.badRanges = [
          { src_offset: 412334080 * 512 + 61440, dst_offset: 412334080 * 512 + 61440, len: 1536, lba: 412334200, sectors: 3, attempts: 4, error: T("Erreur de données (contrôle de redondance cyclique)", "Data error (cyclic redundancy check)"), class: "media", outcome: "pending" },
          { src_offset: 698112512 * 512, dst_offset: 698112512 * 512, len: 2048, lba: 698112512, sectors: 4, attempts: 4, error: T("Erreur de données (contrôle de redondance cyclique)", "Data error (cyclic redundancy check)"), class: "media", outcome: "pending" },
        ];
        if (job.params.unreadable === "ask") {
          emit("clone-ask", { ranges: job.badRanges, total_bytes: 3584, total_sectors: 7, sector_size: 512 });
          decision = await new Promise((r) => (job.answer = r));
        } else decision = job.params.unreadable === "stop" ? "stop" : "zero";
        if (decision === "stop") { log("error", T("Arrêt demandé : secteurs illisibles sur la source", "Stop requested: unreadable sectors on the source")); ok = false; job.stopUnreadable = true; }
        else { job.badRanges.forEach((b) => (b.outcome = "zero_filled")); log("warning", T("7 secteur(s) illisible(s) remplacé(s) par des zéros sur la destination", "7 unreadable sector(s) replaced with zeros on the destination")); }
      }
    }
    if (job.fail) log("error", T("La destination ne répond plus (périphérique déconnecté) après 2 nouvelles tentatives d'écriture", "The destination no longer responds (device disconnected) after 2 write retries"));
    let tableOk = false;
    if (ok) {
      if (job.params.verify !== "none") { log("info", T("Vérification des données copiées", "Verifying copied data")); ok = await phase("verifying", total, 7); }
      const g = job.prep.plan.grow;
      const sh = job.prep.plan.shrink;
      if (ok && sh) {
        log("info", T("Réduction de la partition Windows", "Shrinking the Windows partition"));
        emit("clone-progress", { phase: "shrinking", done: 0, total: 0, elapsed_ms: 0, speed_bps: 0, avg_bps: 0, eta_secs: null, read_errors: job.err.read, bad_bytes: job.err.bad, recovered_bytes: job.err.rec, write_errors: 0, current_lba: 0 });
        await wait(1300);
        log("info", T(`Partition ${sh.number} réduite : ${GB(sh.old_len)} → ${GB(sh.new_len)}`, `Partition ${sh.number} shrunk: ${GB(sh.old_len)} → ${GB(sh.new_len)}`));
      }
      if (ok && g) {
        log("info", T("Agrandissement de la partition Windows", "Growing the Windows partition"));
        emit("clone-progress", { phase: "growing", done: 0, total: 0, elapsed_ms: 0, speed_bps: 0, avg_bps: 0, eta_secs: null, read_errors: job.err.read, bad_bytes: job.err.bad, recovered_bytes: job.err.rec, write_errors: 0, current_lba: 0 });
        await wait(1300);
        log("info", T(`Partition ${g.number} agrandie : ${GB(g.old_len)} → ${GB(g.new_len)}`, `Partition ${g.number} grown: ${GB(g.old_len)} → ${GB(g.new_len)}`));
      }
      // As in the engine, the partition table is the last write.
      log("info", T("Écriture de la table de partitions", "Writing the partition table"));
      emit("clone-progress", { phase: "writing_table", done: 0, total: 0, elapsed_ms: 0, speed_bps: 0, avg_bps: 0, eta_secs: null, read_errors: job.err.read, bad_bytes: job.err.bad, recovered_bytes: job.err.rec, write_errors: 0, current_lba: 0 });
      await wait(900);
      tableOk = true;
      if (ok) { log("info", T("Contrôle des structures du clone", "Checking the clone's structures")); emit("clone-progress", { phase: "checking", done: 0, total: 0, elapsed_ms: 0, speed_bps: 0, avg_bps: 0, eta_secs: null, read_errors: job.err.read, bad_bytes: job.err.bad, recovered_bytes: job.err.rec, write_errors: 0, current_lba: 0 }); await wait(1500); }
    }
    if (job.cancel && !job.fail) log("warning", T("Clonage annulé par l'utilisateur", "Clone cancelled by the user"));
    finish(ok, tableOk);
  }

  function finish(ok, tableOk) {
    const rep = clone(D().report), o = rep.outcome, plan = job.prep.plan, total = plan.total_copy_bytes;
    rep.source = job.prep.source; rep.destination = job.prep.destination; rep.plan = plan; rep.precheck = job.prep.findings;
    rep.smart_source = job.prep.smart_source; rep.smart_destination = job.prep.smart_destination;
    rep.id = new Date().toISOString().slice(0, 19).replace("T", "_").replace(/:/g, "").replace(/-(\d\d)$/, "$1");
    rep.started_at = new Date(job.t0).toLocaleString(MLANG === "en" ? "en-GB" : "fr-FR").replace(",", "");
    o.duration_ms = Date.now() - job.t0;
    const copied = job.cancel ? Math.round(total * 0.45) : total;
    Object.assign(o.copy, { bytes_planned: total, bytes_copied: copied, bytes_zero_filled: job.badRanges && !job.stopUnreadable ? 3584 : 0, read_error_events: job.err.read, write_error_events: job.fail ? 3 : 0, recovered_bytes: job.err.rec, copy_ms: 14000, avg_bps: Math.round(total / 14) });
    o.bad_ranges = job.badRanges || [];
    o.recovered = job.err.rec ? [{ lba: 412334080, sectors: 120, attempt: 1 }, { lba: 412334203, sectors: 5, attempt: 2 }] : [];
    o.table_written = tableOk;
    const g = plan.grow;
    o.grow = g ? (ok ? { number: g.number, attempted: true, done: true, modified: false, error: null, summary: { old_size: g.old_len - 512, new_size: g.new_len - 512, cluster_size: 4096, old_clusters: 0, new_clusters: 0, bitmap_extra_clusters: 1 } } : { number: g.number, attempted: false, done: false, modified: false, error: T("non tenté", "not attempted"), summary: null }) : null;
    const sh = plan.shrink;
    o.shrink = sh && ok ? { number: sh.number, attempted: true, done: true, error: null, summary: { old_size: sh.old_len - 512, new_size: sh.new_len - 512, cluster_size: 4096, old_clusters: 0, new_clusters: 0, relocated_clusters: Math.round(sh.relocated_bytes / 4096), records_updated: 1840, used_clusters: 0 } } : null;
    if (!ok) {
      o.table_check = { performed: false, ok: false, problems: [] };
      o.data_check = { mode: "none", total_bytes: total, checked_bytes: 0, completed: false, mismatches: [], duration_ms: 0 };
      o.boot_destination = null;
      if (job.fail) { o.status = "failed"; o.stop = { kind: "destination_failed" }; o.reasons = [T("La destination a cessé de répondre pendant la copie (périphérique déconnecté). Le clone est inutilisable : il n'a pas reçu de table de partitions.", "The destination stopped responding during the copy (device disconnected). The clone is unusable: it received no partition table."), T("La source n'a pas été modifiée.", "The source has not been modified.")]; }
      else if (job.stopUnreadable) { o.status = "incomplete"; o.stop = { kind: "unreadable" }; o.reasons = [T("Clonage arrêté à votre demande : 7 secteurs de la source sont illisibles.", "Cloning stopped at your request: 7 source sectors are unreadable."), T("La destination n'a pas reçu de table de partitions : elle est inutilisable en l'état.", "The destination received no partition table: it is unusable as it stands.")]; }
      else { o.status = "incomplete"; o.stop = { kind: "cancelled" }; o.reasons = [T("Clonage annulé avant la fin : environ 45 % des données ont été copiées.", "Clone cancelled before the end: about 45% of the data was copied."), T("La destination n'a pas reçu de table de partitions : elle est inutilisable en l'état. Son ancien contenu n'est pas récupérable.", "The destination received no partition table: it is unusable as it stands. Its previous content cannot be recovered.")]; }
    } else if (job.badRanges) {
      o.status = "incomplete";
      o.reasons = [T("7 secteurs illisibles de la source (3,5 Ko) ont été remplacés par des zéros : les fichiers qui les utilisaient sont endommagés sur le clone.", "7 unreadable source sectors (3.5 KB) were replaced with zeros: the files that used them are damaged on the clone."), T("Le reste des données a été copié puis relu sans différence ; la table de partitions et les systèmes de fichiers du clone sont conformes.", "The rest of the data was copied then read back without differences; the clone's partition table and file systems are correct.")];
      o.data_check.total_bytes = o.data_check.checked_bytes = total;
    } else if (job.params.verify === "none") {
      o.status = "completed_with_warnings";
      o.data_check = { mode: "none", total_bytes: total, checked_bytes: 0, completed: false, mismatches: [], duration_ms: 0 };
      o.reasons = [T("La copie s'est terminée sans erreur, mais les données n'ont pas été relues (vérification désactivée) : le résultat ne peut pas être déclaré vérifié.", "The copy finished without errors, but the data was not read back (verification disabled): the result cannot be reported as verified.")];
    } else {
      o.data_check.total_bytes = o.data_check.checked_bytes = total;
      o.data_check.mode = job.params.verify;
    }
    if (o.grow && o.grow.done) o.reasons = o.reasons.concat([T(`La partition ${o.grow.number} a été agrandie de ${GB(o.grow.summary.old_size)} à ${GB(o.grow.summary.new_size)} ; Windows vérifiera ce volume (chkdsk) une fois, au premier démarrage du clone.`, `Partition ${o.grow.number} was grown from ${GB(o.grow.summary.old_size)} to ${GB(o.grow.summary.new_size)}; Windows will check this volume (chkdsk) once, on the clone's first boot.`)]);
    rep.log =job.log.concat([{ t_ms: o.duration_ms, time: now(), level: o.status === "success_verified" ? "info" : "warning", message: T("Résultat : ", "Result: ") + (MLANG === "en" ? { success_verified: "Clone succeeded and verified", completed_with_warnings: "Clone completed with warnings", incomplete: "Clone incomplete", failed: "Clone failed" } : { success_verified: "Clonage réussi et vérifié", completed_with_warnings: "Clonage terminé avec avertissements", incomplete: "Clonage incomplet", failed: "Clonage échoué" })[o.status] }]);
    reports.unshift(rep);
    const j = job, j0 = job; job = null;
    const ej = j0.params.eject && ["success_verified", "completed_with_warnings"].includes(o.status) && j0.params.destination_id.startsWith("disk:");
    emit("clone-done", { report: rep, error: null, blocked: null, ejected: ej ? true : null, eject_error: null });
    return j;
  }

  // ----------------------------------------------------------------- Rapports
  const reports = [];
  (function seed() {
    const r = clone(D().report);
    r.id = "2026-11-02_213512"; r.started_at = "2026-11-02 21:35:12";
    r.source = disks[2]; r.destination = mk({ number: 9, model: "Seagate Expansion HDD", serial: "NAAB12CD", bus: "USB", media: "hdd", size: 4000787030016, removable: true });
    r.outcome.status = "completed_with_warnings";
    r.outcome.reasons = [T("Copie et relecture sans erreur.", "Copy and read-back without errors."), T("Le disque source était marqué « à vérifier » par Windows (volume sale) : sa partition a été copiée secteur par secteur.", "The source disk was marked \"to be checked\" by Windows (dirty volume): its partition was copied sector by sector.")];
    r.outcome.duration_ms = 3 * 3600e3 + 12 * 60e3;
    r.outcome.copy.bytes_copied = r.outcome.copy.bytes_planned = wd - 2 * MiB;
    r.outcome.copy.avg_bps = 172e6;
    reports.push(r);
  })();
  const summary = (r) => ({ id: r.id, started_at: r.started_at, status: r.outcome.status, source: r.source.model, destination: r.destination.model, bytes_copied: r.outcome.copy.bytes_copied, duration_ms: r.outcome.duration_ms });

  // ---------------------------------------------------------------------- API
  const cmds = {
    env_info: () => ({ name: "Chud Clone", version: "0.9.2", elevated: ADMIN, portable: true, data_dir: "E:\\ChudClone\\ChudClone-data", reports_dir: "E:\\ChudClone\\ChudClone-data\\reports", physical_write_validated: true, confirm_word: "EFFACER", system_lang: Q.get("lang") === "en" ? "en" : "fr" }),
    set_language: ({ lang }) => { MLANG = lang === "en" ? "en" : "fr"; setDisks(); return null; },
    list_disks: async () => { await wait(700); return clone(disks); },
    read_smart: async ({ id }) => { await wait(600); return smartOf(disks.find((d) => d.id === id)); },
    disk_health: async ({ id }) => {
      await wait(400);
      const d = disks.find((x) => x.id === id), smart = smartOf(d);
      if (!smart.available || !d.serial) return { smart, history: { snapshots: [] }, trends: [] };
      const bad = SCEN === "bad" && d.number === 1, now = Date.now() / 1000;
      const snaps = Array.from({ length: d.number === 3 ? 1 : 14 }, (_, i) => ({
        at: Math.round(now - (13 - i) * 14 * 86400), level: "good", temperature_c: (smart.temperature_c || 35) - 3 + Math.round(Math.sin(i * 1.7) * 4),
        reallocated: bad ? (i < 6 ? 0 : i < 10 ? 8 : 24) : 0, pending: bad ? (i < 11 ? 0 : (i - 10) * 3) : 0, crc_errors: d.number === 2 && i > 8 ? 2 : 0,
      }));
      const trends = bad ? [
        { severity: "error", code: "TREND_PENDING", title: T(`Secteurs en attente de réallocation : 0 → 9 depuis le ${new Date(snaps[0].at * 1000).toLocaleDateString("fr-FR")}`, `Sectors pending reallocation: 0 → 9 since ${new Date(snaps[0].at * 1000).toISOString().slice(0, 10)}`), detail: T("Le disque rencontre de nouveaux secteurs qu'il n'arrive plus à lire. C'est le signe le plus net d'une surface qui se dégrade : copiez vos données ou clonez ce disque sans attendre.", "The disk keeps finding new sectors it can no longer read. This is the clearest sign of a degrading surface: copy your data or clone this disk without waiting."), blocking: false, scope: null, lba: null },
        { severity: "warning", code: "TREND_REALLOCATED", title: T(`Secteurs réalloués : 0 → 24 depuis le ${new Date(snaps[0].at * 1000).toLocaleDateString("fr-FR")}`, `Reallocated sectors: 0 → 24 since ${new Date(snaps[0].at * 1000).toISOString().slice(0, 10)}`), detail: T("Le disque a remplacé de nouveaux secteurs défectueux par des secteurs de réserve.", "The disk has replaced new faulty sectors with spare ones."), blocking: false, scope: null, lba: null },
      ] : d.number === 2 ? [{ severity: "warning", code: "TREND_CRC", title: T("Erreurs de transmission : 0 → 2", "Transmission errors: 0 → 2"), detail: T("Ces erreurs viennent en général du câble, du connecteur ou du boîtier USB.", "These errors usually come from the cable, the connector or the USB enclosure."), blocking: false, scope: null, lba: null }] : [];
      return { smart, history: { model: d.model, serial: d.serial, size: d.size, snapshots: snaps }, trends };
    },
    inspect_disk: async ({ id }) => {
      await wait(900);
      const d = disks.find((x) => x.id === id);
      if (!ADMIN && d.kind === "physical") throw T("Lire directement ce disque nécessite les droits administrateur.", "Reading this disk directly requires administrator rights.");
      const boot = d.table === "gpt" && d.partitions.length > 1 ? clone(D().prep.boot) : { style: "data_only", checks: [], findings: [], windows_partition: null, esp_partition: null };
      const findings = d.number === 2 ? [{ severity: "warning", code: "NTFS_DIRTY", title: T("Volume NTFS marqué « à vérifier »", "NTFS volume marked \"to be checked\""), detail: T("Windows n'a pas été arrêté proprement ou une erreur a été détectée. Cette partition sera copiée secteur par secteur. Conseil : lancez « chkdsk D: /f » avant de cloner.", "Windows was not shut down cleanly or an error was detected. This partition will be copied sector by sector. Tip: run \"chkdsk D: /f\" before cloning."), blocking: false, scope: "Partition 1", lba: null }] : [{ severity: "info", code: "LAYOUT_OK", title: T("Table de partitions cohérente", "Consistent partition table"), detail: T("Aucune incohérence trouvée.", "No inconsistency found."), blocking: false, scope: null, lba: null }];
      return { disk: d, findings, boot };
    },
    fix_boot: async ({ id }) => {
      const d = disks.find((x) => x.id === id);
      if (d.is_system) throw T("c'est le disque du Windows en cours d'exécution", "this is the disk of the running Windows");
      for (const st of [T("Démarrage : Windows (UEFI / GPT)", "Boot: Windows (UEFI / GPT)"), T("Partition Windows : Z: — partition système : Y:", "Windows partition: Z: — system partition: Y:"), T("Pilotes activés au démarrage : stornvme, storahci", "Drivers enabled at boot: stornvme, storahci"), T("Configuration de démarrage mise à jour (bcdedit)", "Boot configuration updated (bcdedit)")]) { emit("fixboot-step", st); await wait(600); }
      return { bcd_fixed: true, bcd_method: "bcdedit", drivers: ["stornvme", "storahci"], hibernation_cleared: Q.get("hiber") === "1", fallback_added: false, reserved_cleared: [], warnings: [], steps: [] };
    },
    add_image: async () => {
      const img = mk({ id: "image:d:\\images\\sauvegarde-portable.img", kind: "image", path: "D:\\Images\\sauvegarde-portable.img", number: null, model: "sauvegarde-portable.img", serial: "", bus: "Fichier", media: "image", size: 256060514304, physical_sector_size: 512, partitions: winParts(256060514304, 88 * GiB) });
      images.splice(0, images.length, ...images.filter((d) => d.id !== img.id), img); setDisks();
      return img;
    },
    add_new_image: async ({ path, size, sourceId }) => {
      // As in the engine: without a size, the partitions of the source plus 1 MiB.
      const s = [...(disks || []), ...images].find((d) => d.id === sourceId);
      if (size == null) size = s && s.partitions.length ? Math.min(s.size, Math.ceil((Math.max(...s.partitions.map((p) => p.offset + p.size)) + MiB) / MiB) * MiB) : s ? s.size : 0;
      const name = String(path).split(/[\\/]/).pop();
      const img = mk({ id: "image:" + String(path).toLowerCase(), kind: "image", path, number: null, model: name + T(" (à créer)", " (to be created)"), serial: "", bus: "Fichier", media: "image", size, physical_sector_size: 512, table: "none" });
      images.splice(0, images.length, ...images.filter((d) => d.id !== img.id), img); setDisks();
      return img;
    },
    remove_image: async ({ id }) => { images.splice(0, images.length, ...images.filter((d) => d.id !== id)); setDisks(); },
    prepare_clone: async ({ params }) => {
      await wait(1500);
      if (!ADMIN && [params.source_id, params.destination_id].some((i) => i.startsWith("disk:"))) throw T("Lire directement un disque physique nécessite les droits administrateur. Cliquez sur « Passer en administrateur ».", "Reading a physical disk directly requires administrator rights. Click \"Switch to administrator\".");
      return buildPrep(params);
    },
    start_clone: async ({ params, confirmation }) => {
      if (!["EFFACER", "ERASE"].includes(confirmation)) throw T("Confirmation incorrecte.", "Wrong confirmation.");
      if (job) throw T("Un clonage est déjà en cours.", "A clone is already running.");
      const prep = buildPrep(params);
      if (prep.blocked) throw T("Les pré-vérifications interdisent ce clonage.", "The pre-checks forbid this clone.");
      job = { params, prep, t0: Date.now(), log: [], err: { read: 0, bad: 0, rec: 0, write: 0 }, cancel: false };
      setTimeout(runJob, 50);
    },
    set_keep_awake: () => null,
    app_state: () => ({ activity: job ? "cloning" : "idle", keep_awake: true }),
    shutdown_pc: async () => { throw T("Banc d'essai : extinction simulée.", "Test bench: simulated shutdown."); },
    format_check: async ({ id, request }) => {
      await wait(300);
      const d = disks.find((x) => x.id === id);
      const max = { ntfs: 32, exfat: 15, fat32: 11 }[request.fs];
      const refusals = [];
      if (d.kind !== "physical") refusals.push(T("seuls les disques peuvent être formatés (pas les fichiers image)", "only disks can be formatted (not image files)"));
      if (d.is_system) refusals.push(T("ce disque porte le Windows en cours d'exécution", "this disk holds the running Windows"));
      if (request.label.length > max) refusals.push(T(`nom trop long pour ${request.fs} : ${max} caractères au plus`, `name too long for ${request.fs}: ${max} characters at most`));
      if (request.fs === "fat32" && /[^ -~]/.test(request.label)) refusals.push(T("caractère interdit dans un nom FAT32", "character not allowed in a FAT32 name"));
      return { disk: d, label: request.fs === "fat32" ? request.label.toUpperCase() : request.label, refusals, own_fat32: request.fs === "fat32" && d.size > 32 * GiB };
    },
    format_disk: async ({ id, request, confirmation }) => {
      if (!["EFFACER", "ERASE"].includes(confirmation)) throw T("Confirmation incorrecte.", "Wrong confirmation.");
      const d = disks.find((x) => x.id === id);
      for (const st of [T(`Effacement du disque ${d.number} et nouvelle table ${request.table.toUpperCase()}`, `Erasing disk ${d.number} and creating a new ${request.table.toUpperCase()} table`), T(`Formatage ${request.fs.toUpperCase()} par Windows`, `${request.fs.toUpperCase()} formatting by Windows`)]) { emit("format-step", st); await wait(900); }
      emit("format-step", T("Volume prêt : F:\\", "Volume ready: F:\\"));
      return { fs: request.fs, table: request.table, label: request.fs === "fat32" ? request.label.toUpperCase() : request.label, partition_size: d.size - 2 * MiB, mount: "F:\\", steps: [] };
    },
    start_scan: async ({ id }) => {
      const d = disks.find((x) => x.id === id);
      const n = Math.min(2400, Math.ceil(d.size / MiB)), cells = new Array(n).fill(255);
      const bad = SCEN === "bad" && d.number === 1;
      const counts = [0, 0, 0, 0, 0, 0, 0], badList = [], slowest = [];
      let i = 0, cancelled = false;
      scanCtl = { cancel: () => (cancelled = true) };
      const t0 = Date.now(), blocksPerCell = Math.ceil(d.size / MiB) / n;
      const tick = () => {
        const upd = [];
        for (let k = 0; k < 40 && i < n; k++, i++) {
          const pos = i / n;
          let c = Math.random() < 0.97 ? 0 : 1;
          if (d.media === "hdd" && Math.random() < 0.01) c = 2;
          if (bad && Math.abs(pos - 0.41) < 0.006) c = pos > 0.4085 && pos < 0.4115 ? 6 : 4 + (Math.random() < 0.4 ? 1 : 0);
          if (bad && Math.abs(pos - 0.73) < 0.002) c = 3 + Math.floor(Math.random() * 3);
          cells[i] = c; counts[c] += Math.round(blocksPerCell * (c === 0 ? 0.97 : 0.03)) || 1; upd.push([i, c]);
          const lba = Math.round(pos * d.size / 512);
          if (c === 6) badList.push({ lba, sectors: 8, error: T("Erreur de données (contrôle de redondance cyclique)", "Data error (cyclic redundancy check)") });
          if (c >= 4 && c < 6) slowest.push({ lba, ms: c === 4 ? 210 + Math.round(Math.random() * 200) : 640 + Math.round(Math.random() * 900) });
        }
        const done = Math.min(d.size, Math.round((i / n) * d.size)), secs = (Date.now() - t0) / 1000 || 0.1;
        emit("scan-progress", { done, total: d.size, speed_bps: Math.round(done / secs), eta_secs: Math.round(((n - i) / 40) * 0.12 / FAST), counts, bad_sectors: badList.length * 8, current_lba: Math.round(done / 512), cells: upd });
        if (i < n && !cancelled) return void setTimeout(tick, 120 / FAST);
        scanCtl = null;
        const bs = badList.length * 8, vs = counts[4] + counts[5];
        const findings = [];
        if (cancelled) findings.push({ severity: "info", code: "SCAN_CANCELLED", title: T(`Scan interrompu à ${((i / n) * 100).toFixed(1)} %`, `Scan interrupted at ${((i / n) * 100).toFixed(1)}%`), detail: T("Les résultats ne portent que sur la partie lue.", "The results only cover the part that was read."), blocking: false, scope: null, lba: null });
        if (bs) findings.push({ severity: "error", code: "SCAN_BAD_SECTORS", title: T(`${bs} secteur(s) illisible(s)`, `${bs} unreadable sector(s)`), detail: T("Ces secteurs ne se relisent pas, même après nouvelles tentatives. Les fichiers qui les utilisent sont endommagés. Sauvegardez ou clonez ce disque.", "These sectors cannot be read back, even after retries. The files that use them are damaged. Back up or clone this disk."), blocking: false, scope: null, lba: null });
        if (vs) findings.push({ severity: "warning", code: "SCAN_SLOW", title: T(`${vs} zone(s) très lente(s) (≥ 150 ms)`, `${vs} very slow area(s) (≥ 150 ms)`), detail: T("Le disque a dû insister pour relire ces zones. Sur un disque dur, c'est souvent le signe précurseur de secteurs qui deviendront illisibles.", "The disk had to insist to read these areas. On a hard disk, this often heralds sectors that will become unreadable."), blocking: false, scope: null, lba: null });
        if (!cancelled && !bs && !vs) findings.push({ severity: "info", code: "SCAN_CLEAN", title: T("Toute la surface a été relue sans erreur ni lenteur notable", "The whole surface was read without errors or notable slowness"), detail: T("Ce résultat décrit l'état actuel ; il ne garantit pas l'absence de panne future.", "This result describes the current state; it does not rule out a future failure."), blocking: false, scope: null, lba: null });
        const scanned = Math.round((i / n) * d.size);
        emit("scan-done", { id, disk: d, error: null, saved: "E:\\ChudClone\\ChudClone-data\\analyses\\scan-2026-11-13_120000.json", result: { size: d.size, sector_size: 512, block_size: MiB, scanned_bytes: scanned, cells, counts, bad: badList, bad_sectors: bs, recovered_blocks: 0, slowest: slowest.sort((a, b) => b.ms - a.ms).slice(0, 100), duration_ms: DEMO ? Math.round(scanned / 512e6 * 1000) : Date.now() - t0, avg_bps: DEMO ? 512e6 : Math.round(scanned / ((Date.now() - t0) / 1000)), end: cancelled ? { kind: "cancelled" } : { kind: "completed" }, findings } });
      };
      setTimeout(tick, 100);
    },
    cancel_scan: () => { if (scanCtl) scanCtl.cancel(); },
    cancel_clone: () => { if (job) { job.cancel = true; if (job.answer) job.answer("stop"); } },
    answer_unreadable: ({ zeroFill }) => { if (job && job.answer) { const a = job.answer; job.answer = null; a(zeroFill ? "zero" : "stop"); } },
    clone_running: () => !!job,
    list_reports: async () => { await wait(300); return reports.map(summary); },
    get_report: async ({ id }) => clone(reports.find((r) => r.id === id)),
    export_report: async () => {},
    relaunch_elevated: async () => { location.search = location.search.replace(/admin=0&?/, ""); },
    // ?update=1: a newer version is published; ?update=fail: its download fails the checksum.
    update_check: async () => {
      await wait(400);
      const avail = Q.has("update");
      return { current: "0.6.0", latest: avail ? "0.6.1" : "0.6.0", available: avail, title: avail ? "Chud Clone 0.6.1" : "Chud Clone 0.6.0", notes: T("Nouveautés :\n- Recherche de mises à jour et mise à jour depuis le logiciel.", "What's new:\n- Update check and in-app update."), published: "2026-11-13T12:00:00Z", page_url: "https://github.com/example/releases/tag/v0.6.1", method: "portable", download_size: 6.1e6 };
    },
    update_install: async () => {
      const total = 6.1e6;
      (async () => {
        for (let d = 0; d <= total; d += total / 20) { emit("update-progress", { phase: "download", done: Math.round(d), total }); await wait(80); }
        emit("update-progress", { phase: "verify", done: total, total });
        await wait(600);
        if (Q.get("update") === "fail") return emit("update-failed", T("le fichier téléchargé ne correspond pas à la somme de contrôle publiée : il a été supprimé", "the downloaded file does not match the published checksum: it has been deleted"));
        emit("update-progress", { phase: "install", done: 0, total: 0 });
      })();
    },
    open_release_page: async () => {},
  };
  const win = { minimize() {}, toggleMaximize() {}, close() { if (job) emit("close-blocked", null); } };
  window.__MOCK__ = {
    invoke: async (cmd, args) => {
      if (!cmds[cmd]) throw T(`commande inconnue : ${cmd}`, `unknown command: ${cmd}`);
      return cmds[cmd](args || {});
    },
    listen: async (ev, h) => { (handlers[ev] = handlers[ev] || []).push(h); return () => {}; },
    open: async () => "D:\\Images\\sauvegarde-portable.img",
    save: async (o) => "D:\\Images\\" + (o.defaultPath || "clone.img"),
    win: () => win,
  };
})();
