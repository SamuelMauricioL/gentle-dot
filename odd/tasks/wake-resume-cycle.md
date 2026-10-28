# Wake word: the detector survives every note

Bug: after using the wake word several times the microphone goes quiet on its own and the wake word
stops working. Fix: one capture at a time, a paused detector, and a resume that cannot be lost.

## Specs

- S1 — "despues de usar el wake word varias veces, el microfono se apaga solo y el wake word deja de
  funcionar": saying the phrase N times in a row starts N notes, and after each one the detector is
  listening again. It never stays dead until the switch is toggled.
- S2 — "(1) al detectar la wake word, el canal del detector se PAUSA/CIERRA momentaneamente": on
  detection the detector capture is closed before the note opens.
- S3 — "(2) arranca el canal de la nota de voz y el pip": the note starts through the panel's own path
  and the pip plays once.
- S4 — "(3) al terminar la nota (corte por silencio), se cierra el canal de la nota y se REANUDA el
  detector": silence end closes the note capture, then the detector re-arms.
- S5 — "(4) nunca dos capturas abiertas a la vez": at no instant are two captures open, including the
  window where the detector's open lands while a note is starting.
- S6 — "(5) loguear cada transicion (detector pausado / nota iniciada / nota cerrada / detector
  reanudado) en wake-word.log, y si la reanudacion no ocurre, loguear el motivo": one line each,
  `paused`, `note_started`, `note_stopped`, `resumed`, and `resume_failed` carrying the reason.
- S7 — "(6) cubrir con tests el ciclo completo usando fakes (varias notas seguidas sin perder el
  detector)": several consecutive notes in one test, asserting the detector is armed after each and
  that the two-captures invariant held throughout.

## Tasks

| ID | Specs | Route | Commit |
|----|-------|-------|--------|
| T1 ✅ `d9390ec` | S2, S5 | `machine.rs`: a `Closing` phase that gates `arm` and `begin`, closed synchronously on the caller's thread, so a capture is never opened while another closes; `stop` on a `Starting` session returns the slot to `Idle` instead of leaving `Starting{cancelled:true}`; a public "is the detector listening" query | — |
| T2 ✅ `afe8cc1` | S1, S6 | `app.rs`: log `paused`/`note_started`/`note_stopped`/`resumed`, and `resume_failed` with its reason; a note that fails to start re-arms the detector instead of stranding it; a `Busy`/`Cancelled` resume schedules a retry; a watchdog re-arms when enabled and not listening | — |
| T3 ✅ `afe8cc1` | S7 | `fake.rs` + tests: capture history to prove S5, per-note recognizer text, detector re-arm counter, and the consecutive-notes cycle | — |
| T4 | S1-S7 | build, install, commit | — |

## Log

- L1 — "Bug reportado por el usuario: despues de usar el wake word varias veces, el microfono se
  apaga solo y el wake word deja de funcionar. Hipotesis de los otros agentes: el canal de audio del
  detector compite con el de la nota de voz y alguno termina cerrandose. Comportamiento IDEAL que pide
  el usuario (implementalo asi): (1) al detectar la wake word, el canal del detector se PAUSA/CIERRA
  momentaneamente; (2) arranca el canal de la nota de voz y el pip; (3) al terminar la nota (corte por
  silencio), se cierra el canal de la nota y se REANUDA el detector; (4) nunca dos capturas abiertas a
  la vez; (5) loguear cada transicion (detector pausado / nota iniciada / nota cerrada / detector
  reanudado) en wake-word.log, y si la reanudacion no ocurre, loguear el motivo; (6) cubrir con tests
  el ciclo completo usando fakes (varias notas seguidas sin perder el detector)."
- L2 — Exploration (task mvpgjnst): VIABLE WITH CHANGES. `Starting`/`Stopping` already encode the
  blocking phases, so the machine needs a `Closing` phase rather than a 5-state enum; `Paused` is
  app-side intent (`enabled` + `armed: None`). `Stream::close()` is synchronous (`macos.rs:272-283`),
  so `Closing` completes on the caller's thread; both `arm` (`machine.rs:289`) and `begin` (`398`)
  open outside the lock today, which is how two captures can briefly coexist. Logging stays in
  `app.rs` because `Wake::log` lives there. Fakes need an open/close history, per-note recognizer text
  and a re-arm counter.
- L3 — Exploration (task mvpgjeej): the lost resume is real but NOT the dominant disarmer. When the
  woken note fails to start (`app.rs:531-535` `record()` propagates the error with `?`, `handle` only
  logs at `521-523`), the machine is already back to `Idle{armed: None}` while `enabled` stays true, so
  the detector is disarmed for good, the mic is closed and `status()` still reports available. Two more
  silent paths: `stop` on a `Starting` session returns `Starting{cancelled:true}` (`machine.rs:463-467`)
  so the next `arm` answers `Busy`, swallowed at `app.rs:496`; and a fresh detector is loaded on every
  resume (`app.rs:457-463`), which costs seconds of silence per note.
- L4 — Decision: implement the user's cycle (S2-S7) as the shape of the fix, and close the three
  stranding paths above, because the cycle alone does not remove them.
- L5 — Implemented. `machine.rs`: `State::Closing { next }` runs `close_listener` synchronously on
  the caller's thread and both `arm` and `begin` answer `Busy` while it is in flight; `disarm`,
  `begin` and the late arm inside `begin` all go through it; `Voice::listening()` answers whether a
  detector capture is open. `app.rs`: `paused`, `note_started`, `note_stopped`, `resumed` and
  `resume_failed {reason, attempt, retry}` are one JSON line each; a note that fails to start logs
  `note_failed` and re-arms; a deferred `Busy`/`Cancelled` resume retries instead of assuming
  another path will; the ladder keeps its slowest step (8 s) instead of giving up, so no single lost
  resume can leave the detector dead. `fake.rs`: ordered open/close `history`, an `overlap` counter,
  one `sinks` entry per capture and `saying_each` for one transcript per note.
- L6 — Evidence: `several_notes_in_a_row_keep_the_detector_listening` runs three notes and asserts
  three of each transition, no `resume_failed`, the transcripts in order, `overlap == 0` and the
  capture history (7 opens, 6 closes: the detector still owns the microphone).
  `a_note_that_never_starts_gives_the_detector_back` is red without the re-arm (`the detector listens
  again` times out) and green with it, which is the reported bug reproduced.
- L7 — Follow-ups, each from a real session's log: `e488382` a note that hears nothing ends on the
  wall clock; `0e9a404` a note stuck starting is cancelled; `8d99e23` every declined resume logs
  `resume_skipped` and a switch the disk says is on is restored; `eb9073f` the engine is never asked
  for the device it already uses; `208fdc1` the listen channel is watched (8 s without a frame → 3
  retries with backoff → the system default for the session); `ec60e87` a note that outlives its
  deadline says why; `2a8c61b` an empty transcript still reaches the panel, which is the freeze the
  user saw on the second note (frozen timer, dead buttons, microphone already closed).
