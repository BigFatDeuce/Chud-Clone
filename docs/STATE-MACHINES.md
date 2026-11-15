# State machines of Chud Clone

Every place where the program is "in a state" is listed here: what the states are, where
they are defined, and what may follow what. If you add a screen, an operation or a
long-running command, start here.

The rule followed everywhere: **a state is one named value, in one place**. A state is
never rebuilt from a set of flags (`running`, `done`, `error`…) that could contradict each
other.

## Reading the state of a running application

| Where | How | What you get |
| --- | --- | --- |
| Interface (application or test bench) | `ChudState()` in the web console | view, activity, state of every machine, undeclared transitions seen, clone queue, analysis of each destination |
| Interface, from the page itself | attributes of `<html>`: `data-clone-state`, `data-format-state`, `data-scan-state`, `data-update-state`, `data-shutdown-state`, `data-activity`, `data-current-view` | the same states, readable by a stylesheet, a test or a tool driving the page |
| Application back end | Tauri command `app_state` | `{ activity, keep_awake }` |
| Full-screen interface (rescue key) | `App::check_invariants()` (tests) | `Err` naming the screen and what is missing |

Example, in the test bench: wait for a clone to finish with
`ChudState().states.clone === "result"` instead of looking for a piece of text.

## 1. Application back end: the activity

`crates/app/src/main.rs`: `enum Activity`, held by `ActivitySlot`.

```text
            begin(Cloning)      ┌──────────┐
        ┌──────────────────────▶│ cloning  │──┐
        │   begin(Scanning)     ├──────────┤  │
        ├──────────────────────▶│ scanning │──┤
 ┌──────┴─┐ begin(Formatting)   ├──────────┤  │ end()
 │  idle  │────────────────────▶│formatting│──┤
 └──────▲─┘ begin(Updating)     ├──────────┤  │
        │ ─────────────────────▶│ updating │──┤
        │                       └──────────┘  │
        └─────────────────────────────────────┘
```

- One activity at a time. `ActivitySlot::begin` is the **only** place where "may this
  start?" is decided: it takes the lock once, refuses with the reason when anything else
  is running, otherwise records the new activity. There is no window between the check
  and the start.
- `end()` goes back to idle, whatever the result (success, failure, panic caught).
- `Cloning` carries what is needed to cancel the clone and to answer its question about
  unreadable sectors; `Scanning` carries what is needed to stop the scan.
- `blocks_close()`: the window cannot be closed during a clone or a format (a disk would
  be left half written).
- Shutting the computer down and restarting as administrator are refused unless idle.

To add an operation: add a variant, give it a name and a "busy" message, start it with
`begin` and finish it with `end`. Nothing else has to know about it.

Keeping the computer awake is separate and simpler: an `AwakeSwitch`
(`crates/core/src/awake.rs`), on while the application is open unless turned off in
Settings (`set_keep_awake`). The engine also holds its own request for the time of a
clone, a scan or a format, so the command line and the rescue key are covered too.

## 2. Interface: `crates/app/ui/machines.js`

Each machine is a table: state → states it may go to. The interface asks `can()` before
acting and moves with `to()`.

An undeclared transition is a bug of the table or of the code. In the test bench it
throws. In the application it is logged, recorded in `ChudState().violations`, and the
move is still made, so that nobody is ever stuck on a screen.

### clone: the wizard

```text
source ⇄ destination ⇄ review → run → result → source
```

- `run` covers the whole queue: the clones of several destinations follow each other
  without leaving it.
- A clone only starts from `review`, once, and never while something else is running.
- When the engine refuses the first clone before anything started (for instance because
  a scan is running), the interface stays on `review` and says why.

### format

```text
idle → checking → ready ⇄ checking
                → refused → checking
       ready → running → done | failed → checking
```

- `checking`: the engine says what it would do and why it would refuse. Any change (disk,
  file system, table, name) goes back to it.
- The confirmation only exists in `ready`.

### scan

```text
idle → running → cancelling → done | failed
       running → done | failed          done | failed → running
```

### update

```text
idle → checking → idle        idle → installing → idle (failure; on success the app restarts)
```

### shutdown (end of a clone queue)

```text
idle → counting → idle (cancelled)        counting → requested
```

### Derived states

They are computed from the data by one function each, never stored twice:

| Function | Values | From |
| --- | --- | --- |
| `activity()` | `idle`, `cloning`, `formatting`, `scanning`, `updating` | the machines above; mirrors the back end's activity |
| `prepState(x)` | `busy`, `error`, `blocked`, `ready` | the analysis of one destination |
| `itemState(it, i)` | `waiting`, `running`, `ok`, `failed` | one clone of the queue |

`busy()` is the single guard used before starting anything from the interface: it shows
why and returns true when something else is running.

### Left as they are, on purpose

- **Loading flags** (`loadingDisks`, `busy[...]` per disk for health and analysis): one
  request, one flag, no sequence to protect.
- **The question about unreadable sectors**: the engine waits for the answer, the modal
  cannot be dismissed without one, and the end of the clone closes it. Its state is the
  engine's.
- **The star of the progress ring** (`Cosmos.Star`, stages `proto → grow → remnant →
  dwarf | dead`): a display that follows the engine's phases; it decides nothing.

## 3. Full-screen interface (rescue key): `crates/cli/src/tui/app.rs`

- `Screen`: `Home`, `Text`, `PickDest`, `Options`, `Review`, `Running`, `Result`, `Scan`,
  `Format`. One screen at a time; a `Modal` may sit on top of it.
- `FormatStage`: `Edit → Running → Done(result)`.
- `shutdown_at`: a shutdown countdown, only on `Result`; Esc cancels it.
- The data of a screen lives in `Option` fields (`prep`, `run`, `report`, `scan`, `fmt`).
  What must go together is written once, in `App::check_invariants()`, and the tests check
  it after **every key and every engine message**:

  | Screen | Requires |
  | --- | --- |
  | `PickDest` | a source |
  | `Options` | a source and a destination |
  | `Review` | an analysis and a destination |
  | `Running` | a running clone and its analysis |
  | `Result` | a report or an error |
  | `Scan` / `Format` | their state |
  | `Home` | no clone, scan or format state left |

  The fields were not folded into the `Screen` enum: it would have rewritten every screen
  for no change in behaviour. The invariants give the same guarantee to whoever edits the
  file, and a failing test names the screen and what is missing.

## 4. Engine: already explicit

| Machine | Where | States |
| --- | --- | --- |
| Clone phases | `engine::Phase` | `Preparing → Copying ⇄ Recovering → Verifying → Growing → WritingTable → Checking → Done` (the table is the last write) |
| Result of a clone | `clone::FinalStatus` | `SuccessVerified`, `CompletedWithWarnings`, `Incomplete`, `Failed`: decided in one function from the facts of the run |
| End of a surface scan | `scan::ScanEnd` | `Completed`, `Cancelled`, `DeviceLost`, `TooManyErrors` |
| Cancellation | `engine::Control` | one flag, set once |
| Unreadable sectors | `engine::Decision` | `ZeroFill` or `Abort`, asked once per clone |

## 5. Rescue key menu: `packaging/winpe/menu.cmd`

A loop between the full-screen interface and the text menu, driven by the exit code of
`chud-clone-cli tui` (`tui::app::exit`):

| Code | Meaning |
| --- | --- |
| 0 | quit: text menu |
| 20 | command prompt, then back to the interface |
| 21 | text menu |
| 22 | restart |
| 23 | shut down (also used by "shut down when done") |
