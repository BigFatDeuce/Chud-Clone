// Chud Clone — explicit state machines of the interface (see docs/STATE-MACHINES.md).
//
// Each machine is a table: state → the states it may go to. The interface asks
// `can()` before acting and moves with `to()`; nothing else changes a state. Every
// state is mirrored on <html> as a data attribute (data-clone-state="review",
// data-format-state="ready"…), so a stylesheet, a test or a tool driving the page can read
// where the application is without guessing from the markup.
//
//   Machines.snapshot()   → { states: { clone: "review", … }, violations: [] }
//   ChudState()           → the same, plus the view, the activity and the clone queue
//                           (defined in app.js)
"use strict";

window.Machines = (() => {
  const all = {};
  const violations = [];
  // In the test bench an undeclared transition is a bug of the table or of the code:
  // it fails loudly. In the application it is logged and recorded, and the move is
  // still made, so the user is never stuck on a screen.
  const strict = !!window.__MOCK__;

  function define(name, spec) {
    const m = {
      name,
      state: spec.initial,
      table: spec.states,
      is: (...states) => states.includes(m.state),
      can: (next) => (spec.states[m.state] || []).includes(next),
      to(next) {
        if (next === m.state) return true;
        if (!(next in spec.states) || !m.can(next)) {
          const v = { machine: name, from: m.state, to: next, at: new Date().toISOString() };
          violations.push(v);
          console.error(`[state] ${name}: ${v.from} → ${next} is not a declared transition`);
          if (strict) throw new Error(`[state] ${name}: ${v.from} → ${next}`);
        }
        m.state = next;
        document.documentElement.dataset[name + "State"] = next;
        return true;
      },
    };
    all[name] = m;
    document.documentElement.dataset[name + "State"] = m.state;
    return m;
  }

  const machines = {
    // The clone wizard. "run" covers the whole queue: the clones of several
    // destinations follow each other without leaving it.
    clone: define("clone", {
      initial: "source",
      states: {
        source: ["destination"],
        destination: ["source", "review"],
        review: ["destination", "run"],
        run: ["result"],
        result: ["source"],
      },
    }),
    // Whole-disk formatting. "checking" asks the engine what it would do and why it
    // would refuse; only "ready" shows the confirmation.
    format: define("format", {
      initial: "idle",
      states: {
        idle: ["checking"],
        checking: ["ready", "refused"],
        ready: ["checking", "running"],
        refused: ["checking"],
        running: ["done", "failed"],
        done: ["checking"],
        failed: ["checking"],
      },
    }),
    // Read-only surface scan (one at a time).
    scan: define("scan", {
      initial: "idle",
      states: {
        idle: ["running", "done", "failed"],
        running: ["cancelling", "done", "failed", "idle"],
        cancelling: ["done", "failed"],
        done: ["running"],
        failed: ["running"],
      },
    }),
    // Update check and installation.
    update: define("update", {
      initial: "idle",
      states: {
        idle: ["checking", "installing"],
        checking: ["idle"],
        installing: ["idle"],
      },
    }),
    // Shutdown at the end of a clone queue: a countdown the user can cancel.
    shutdown: define("shutdown", {
      initial: "idle",
      states: {
        idle: ["counting"],
        counting: ["idle", "requested"],
        requested: ["idle"],
      },
    }),
  };

  function snapshot() {
    const states = {};
    for (const k of Object.keys(all)) states[k] = all[k].state;
    return { states, violations: violations.slice() };
  }

  return Object.assign(machines, { define, snapshot, violations });
})();
