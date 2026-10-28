# Wake word audio: find the root cause before any more patches

Six fixes went in one symptom at a time (deadline, stall, resume_skipped, device gate, capture guard,
empty transcript). Each was real and tested, and the thing still fails: with a Bluetooth headset the
microphone ends up off, toggling the switch does nothing, and captures appear live while delivering
nothing. This task is an audit, not another patch: the design is under suspicion, not just its bugs.

## Specs

- S1 — No code changes until the audit names the root cause with `path:line` evidence and the user
  approves a plan.
- S2 — The audit answers, for this machine: why a capture can be open (orange dot on) and deliver no
  frames; whether the app can observe that today; and which of the six patches are symptoms of the
  same defect.
- S3 — The audit says whether a single long-lived audio engine with a swappable sink is the right
  architecture, and what it costs.
- S4 — The audit lists every actor that can open or close the microphone (guard, deadline, stall,
  retry ladder, panel, machine) and whether their orders can contradict each other.
- S5 — The audit lists every state the panel can be left in that the backend cannot correct.

## Tasks

| ID | Specs | Route | Commit |
|----|-------|-------|--------|
| T1 | S2, S3 | `gentle-ai-explore`: audio backend and device lifecycle (macos.rs, core_audio.rs, machine.rs open/close/arm) | — |
| T2 | S2, S4 | `gentle-ai-explore`: state machine and actors that touch the microphone (machine.rs, app.rs woken path, guard/deadline/stall/retry) | — |
| T3 | S5 | `gentle-ai-explore`: panel contract (voice-native.ts, useVoice.ts, SettingsPanel) | — |
| T4 ✅ | S1, S2 | Synthesize the three reports into one root cause, one plan, and ask before writing code | — |
| T5 ✅ `051e264` | S2, S4 | Supervisor: one owner of `(enabled, capture, delivering)`; the only caller of arm/disarm/resume; every other actor sends it an intent | — |
| T6 ✅ `051e264` | S2 | Liveness for both sinks: a frame count per capture (Listener and Recording), so "open and mute" is observed where it happens | — |
| T7 ✅ `509041a` | S5 | One state contract: `voice://state {wake, capture, mic}` on every transition; the panel mirrors it, and `set` rolls back so the switch never lies | — |
| T9 ✅ (ver Log L7) | S3 | One audio engine for the app's life: open once, swap the sink between detector and note, close only when the switch goes off or the device changes | — |
| T8 | S3 | Device chain in one place (chosen → default → built-in) with the sticky flags reset when frames arrive | — |

## Log

- L1 — "espera, estás implementando por implementar, solo buscando salidas al primer error que te
  diga, tienes que hacer una exploración de la solución completa usando agentes y recién buscar cuál
  puede ser el problema de fondo, si te ayuda usar odd más tasks úsalo"
- L2 — Audit verdicts. T1: `Ok` from `startAndReturnError` only means the engine started, `open` falls
  back to the default only on `Err`, and the frame counters feed only the `Listener` sink, so a note
  has no liveness at all. T2: seven orderings leave the microphone off with the switch on, and the
  flags that cause them are process-sticky, so toggling cannot recover; verdict is one owner, not
  more guards. T3: a wake-started note sets `phase=recording` without a ticker or a `recording`
  entry, so the timer freezes and all three buttons early-return, and `set` leaves the switch lying in
  both directions.
- L3 — Root cause: the switch, the open capture and "delivering frames" are three independent truths
  with no single writer. Every symptom is one of their contradictions.
- L4 — Delivered. One supervisor per app decides listening, recovering or watching a note and is
  the only caller of arm/disarm/resume; the per-note stall, deadline and fallback timers are gone.
  The note's own capture counts frames, so a note that opened mute is observed (`note_deaf`) instead
  of guessed from a clock. `voice://state` carries `{wake, capture}` on every transition and the panel
  mirrors it; `set` rolls back so the switch cannot lie. `4715cf1`: `stop()` defers to the supervisor
  when the machine is still closing, which is what produced a retry ladder of failed arms.
- L5 — Still open: T8 (the device chain now walks chosen → default → built-in inside the supervisor,
  but its transitions deserve a measurement with the headset awake and as the system input, and the
  Bluetooth profile question is a platform limit, not a bug).
- L6 — Measured, not suspected: 16 of the last 18 `resumed` are followed by `capture_lost` within 12 s,
  so a freshly opened capture is mute on this machine. The user approved T9 (one engine, swappable
  sink). Process: an exploration agent checks the plan against the real audio thread first, then a
  worker implements it, then the parent reviews the diff, runs the checks and commits.
- L7 — T9 delivered (exploration `mvqr0g6p`, worker `mvqr3swv`, reviewed by the parent). One engine
  per switch-on with a `SinkSlot` the tap reads per buffer; the capture is a machine resource and
  every note is a `select`, so the mute-capture churn is gone by construction. The parent moved the
  engine open and the stale close out of the capture lock: starting an engine can take seconds, and
  a lock held across it would stall every swap. `cargo test`: 484 passed, 4 ignored. Field checks
  still owed: one engine per switch-on, a device switch while listening, and Apple Speech on notes
  2 and 3.
