// Banc d'essai : amène l'interface dans un état donné, pour les captures d'écran du README.
// ?demo=source|review|run|result|disks  (avec le moteur simulé de mock.js ; aucun disque touché)
"use strict";
(function () {
  const demo = new URLSearchParams(location.search).get("demo");
  if (!demo) return;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const until = async (cond) => { for (let i = 0; i < 400 && !cond(); i++) await sleep(50); };
  window.addEventListener("load", async () => {
    await until(() => S.env && !S.loadingDisks);
    await sleep(300);
    if (demo === "disks") {
      goto("disks");
      await until(() => S.disks.length);
      actions["disk-sel"]({ dataset: { id: "disk:1" } });
      await sleep(1500);
      const scan = document.querySelector('[data-act="scan"]');
      if (scan) actions.scan(scan);
      return;
    }
    if (demo === "source") return;
    S.srcId = "disk:1"; S.step = "destination"; render();
    await sleep(200);
    S.dstId = "disk:3"; S.step = "review"; prepare();
    await until(() => S.prep);
    if (demo === "review") return;
    S.confirmText = confirmWord(); render();
    await sleep(200);
    actions.start($('[data-act="start"]'));
    // Temps écoulé cohérent avec le débit simulé (512 Mo/s).
    if (demo === "run") return void setInterval(() => { if (S.run?.progress?.done) S.run.started = Date.now() - (S.run.progress.done / 512e6) * 1000; }, 50);
    await until(() => S.step === "result");
  });
})();
