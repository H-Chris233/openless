# OpenLess beta — Logic Review (commit 400097ad)

Audit branch: `origin/beta` @ `400097ad` (= "Merge PR #391 fix/audit-async-hygiene").
Sources: `openless-all/app/src-tauri/src/{lib.rs, coordinator.rs, coordinator/{dictation,qa,resources}.rs, coordinator_state.rs, hotkey.rs, audio_mute.rs}`, `openless-all/app/src/lib/{ipc.ts,types.ts}`.

> Caller's premise correction: the prompt asserts "PR #389 (emit_capsule main thread) is pending merge but the same change is already cherry-picked-equivalent on the current code." This is **false** at `400097ad`. Commits `faf02ad4` and `84ee3d96` exist on a side branch but are NOT ancestors of `400097ad` (verified with `git merge-base --is-ancestor`). The audio-thread → AppKit/Win32 SIGTRAP risk that PR #389 was written to fix is therefore **still live on this beta**. See P4.

## Summary

| Path | Verdict | Issues found |
|------|---------|--------------|
| P1 startup | OK | All 6 listeners paired (start in `setup`/`Ready`, stop in `Exit`); tray watcher signaled. |
| P2 press/release | OK with 1 ⚠️ | Routing + dedup correct; `acquire_recording_mute` correctly awaited in dictation start. |
| P3 end-of-session | 1 🚩 | `cancel_session` during Processing leaves `focus_target` set until next `begin_session` overwrites it. PR #387 contract is incomplete for the Processing branch. |
| P4 capsule UI emit | 1 🚩 (CRITICAL) | `emit_capsule` calls `window.show/hide` + `show_capsule_window_no_activate` directly from the cpal audio callback at ~30 Hz on `400097ad`. PR #389 is **not** merged here. |
| P5 shutdown | 1 🚩 | `acquire_recording_mute` at QA path (`coordinator.rs:2313`) is missing `.await` — return value is a dropped Future. PR #391 hygiene fix is incomplete. Compiler emits `unused_must_use`. |

Net: **3 real bugs (🚩)**, 1 cross-PR composition concern, plus minor smells. Two of the three are direct consequences of merge-incomplete state of PR #389/#391. The Processing-cancel `focus_target` leak is a code-path PR #387 missed.

## Findings (per path)

### P1 — Startup

- OK `lib.rs:316-358` — `RunEvent::Exit` stops all 6 hotkey listeners + signals tray watcher; matches the 6 starts at `lib.rs:226` (dictation, in `setup`) and `lib.rs:320-325` (QA / combo / translation / switch_style / open_app, in `RunEvent::Ready`).
- OK `coordinator.rs:344-358, 371-373, 383-385, 395-397, 407-409, 1313-1347` — every `stop_*_listener` for `global-hotkey`-backed monitors marshals the `Drop` to `app.run_on_main_thread`, matching the issue #169 contract for Carbon `RemoveEventHotKey`. `take_combo_hotkey_on_main_thread` / `take_translation_hotkey_on_main_thread` / `take_action_hotkey_on_main_thread` are the helpers; `stop_qa_hotkey_listener` inlines the same pattern.
- OK `coordinator.rs:330-332` + `hotkey.rs:344-355` — dictation `stop_hotkey_listener` is a plain `inner.hotkey.lock().take()`, but the Drop chain is `HotkeyMonitor::drop` → `MacHotkeyAdapter::shutdown` → `CGEventTapEnable(false)` + `CFRunLoopStop(rl)`. PR #388 fix in place; the comment at `hotkey.rs:311-314` correctly notes both APIs are documented thread-safe, so the lack of main-thread marshalling here is intentional.
- ℹ️ `coordinator.rs:316-318` + `global_hotkey_runtime.rs:60-63` — `request_shutdown` is `#[allow(dead_code)]` and never set in production; supervisor loops poll it but nothing flips it (matches PR #392's "passive infrastructure" promise).
- ℹ️ `global_hotkey_runtime.rs:19, 41-55` — `GlobalHotKeyManager` lives in `OnceCell<Arc<...>>` and is therefore never Dropped in production. On macOS this means the Carbon event handler is reaped only at process exit. Acceptable but worth noting if anyone tries to add hot-restart later.

### P2 — Press/release

- OK `coordinator/dictation.rs:11-28` — `handle_pressed_edge` swap-dedups via `inner.hotkey_trigger_held.swap(true, SeqCst)`; routing checks `panel_visible && !dictation_active` (PR #390 fix). The `dictation_active = !matches!(phase, SessionPhase::Idle)` snapshot is the correct guard: it lets a hotkey press during an in-flight dictation flow fall through to `handle_pressed`, even when the QA panel happens to be visible.
- OK `coordinator/dictation.rs:53-67` — `handle_released_edge` symmetric: `panel_visible && !dictation_active → return`. If dictation_active was true when pressed (so it routed to `handle_pressed`), released will also bypass the QA short-circuit and reach `handle_released`. No mismatched-edge leak.
- OK `coordinator/dictation.rs:30-51, 69-85` — Hold/Toggle phase matrix correct: `(Toggle, Idle) → begin`, `(Toggle, Listening) → end`, `(Toggle, Starting) → request_stop_during_starting`, `(Hold, Idle) → begin`, `(Hold, Listening released) → end`, `(Hold, Starting released) → request_stop_during_starting`. Other combinations no-op.
- OK `coordinator/dictation.rs:87-96` + `coordinator_state.rs:87-93` — `request_stop_during_starting_state` only flips `pending_stop` when phase is exactly `Starting`; `finish_starting_session_state` consumes the bit at the Listening transition (`coordinator_state.rs:118-124`) and triggers immediate `end_session` via `BeginOutcome::PendingStop` at `dictation.rs:587-590`.
- OK `coordinator/dictation.rs:451` — `acquire_recording_mute(inner, "dictation").await;` is properly awaited (PR #391 fix applied to dictation path).
- ⚠️ `coordinator/dictation.rs:418-447` (level_handler) — runs on the cpal audio callback thread and calls `emit_capsule` synchronously. See P4 for the concrete bug; flagged here because P2's recorder-start path is the producer.

### P3 — End-of-session pipeline

- OK `coordinator/dictation.rs:595-602` + `coordinator_state.rs:178-184` — `start_processing_if_listening` only transitions `Listening → Processing`; if phase is anything else (`Idle`, `Starting`, `Inserting`, already-Processing), `end_session` returns Ok(()) immediately. Guards against stale pending_stop or duplicate IPC.
- OK `coordinator/dictation.rs:607-620` — recorder + ASR are taken with session-id matching (`take_recorder_for_session` / `take_asr_for_session`), so a stale callback that lands after a session has been re-bumped won't pick up the wrong recorder.
- OK `coordinator/dictation.rs:984-1005` — atomic Inserting transition: same `state.lock()` checks `cancelled` and flips phase to `Inserting`. Once `Inserting`, `cancel_session` rejects (`coordinator_state.rs:155-159` — `Idle | Inserting` → `None`). This is the audit HIGH #2 contract.
- OK `coordinator/dictation.rs:1007-1031` — `paste_shortcut = prefs.paste_shortcut` flows into `inner.inserter.insert(&polished, restore_clipboard, paste_shortcut)` for non-Windows and into `insert_with_windows_ime_first(..., paste_shortcut, ime_target)` for Windows. PR #377 wiring confirmed; corresponding signature in `coordinator.rs:1673-1680, 1731-1745` and `insertion.rs:43-89`.
- OK `coordinator/dictation.rs:1122-1126` — happy-path end_session clears `state.focus_target = None` before scheduling capsule idle.

- 🚩 `coordinator/dictation.rs:843-849` + `coordinator/dictation.rs:1153-1178` + `coordinator_state.rs:171-176` — **`focus_target` is not cleared when cancel hits during `Processing`.**
  - `cancel_session` in the `Processing` branch (`dictation.rs:1171-1173`) deliberately does NOT call `finish_cancel_session_state`, leaving phase + focus_target as-is so `end_session` can finish unwinding.
  - `end_session`'s "ASR-finished, cancelled" exit (`dictation.rs:845-849`) restores Windows IME, sets `phase = Idle`, returns Ok(()) — but never touches `focus_target`.
  - PR #387 (`ce82fcd9`) was framed as "clear `focus_target` on cancel regardless of phase", but the only code path that gained the unconditional clear is `finish_cancel_session_state` at `coordinator_state.rs:172`, which the Processing branch skips.
  - Concrete consequence: between cancel-mid-Processing and the next `begin_session`, the cached AX `focus_target` (a stale `usize` slot) is reachable by anyone reading `state.focus_target` (logs, debug dumps, future readers). It's overwritten by `begin_session_state` at `coordinator_state.rs:80`, so user-visible insertion uses the right value. Severity: minor leak / contract violation rather than user-visible breakage. Tests at `coordinator_state.rs:362-385` only validate the cancel happy paths via `finish_cancel_session_state`, so the regression slipped past PR #387's guard test.

- ⚠️ `coordinator/dictation.rs:1175` — even when cancel fires during `Processing`, the user immediately sees `CapsuleState::Cancelled`, but `end_session` may still be inside the ASR await for several seconds. The phase is still `Processing` until `end_session` reaches the `state.cancelled` check, so a fast retry-press will be quietly dropped (`begin_session_state` requires `Idle`). Not a bug per se — matches the design comment at `dictation.rs:1169-1174` — but worth a UX note.

- ⚠️ `coordinator/dictation.rs:1153-1163` — `cancel_session` swallows the result of `begin_cancel_session_state` for `Inserting`, only logging "cancel ignored". Acceptable, but there's no UI signal back to the user that their Esc didn't take. Minor.

### P4 — Capsule UI emission

- 🚩🚩 **`coordinator.rs:3684-3727`** — **`emit_capsule` does NOT marshal `window.show/hide` to the main thread on `400097ad`.** Verbatim from current source:

  ```rust
  fn emit_capsule(...) {
      ...
      if let Some(window) = app.get_webview_window("capsule") {
          ...
          let visible = !matches!(state, CapsuleState::Idle);
          maybe_position_capsule_bottom_center(inner, &window, payload.translation);
          if show_capsule && visible {
              if !show_capsule_window_no_activate(&app, &window) {
                  let _ = window.show();
              }
              #[cfg(target_os = "macos")]
              crate::restore_main_window_key_if_active(&app);
          } else {
              hide_capsule_window_if_present();
              let _ = window.hide();
          }
      }
      let _ = app.emit_to("capsule", "capsule:state", payload);
  }
  ```

  This is the *pre-PR-#389* shape. PR #389's fix (`faf02ad4`, then `84ee3d96`) wraps the `if let Some(window)` block in `app.run_on_main_thread(move || { ... })`. Verified that neither commit is an ancestor of `400097ad`:

  ```
  $ git merge-base --is-ancestor faf02ad4 400097ad ; echo $?  →  1   (NOT ancestor)
  $ git merge-base --is-ancestor 84ee3d96 400097ad ; echo $?  →  1   (NOT ancestor)
  ```

  Reproduction reasoning: `coordinator/dictation.rs:418-447` builds `level_handler` as `Arc<dyn Fn(f32) + Send + Sync>` and hands it to `Recorder::start`. cpal calls it from the audio process callback thread; the handler then calls `emit_capsule(...)` (line 439-446). On macOS, `WebviewWindow::show()` / `hide()` and the `show_capsule_window_no_activate` (which calls `NSWindow.orderFrontRegardless`) hit AppKit assertions (`dispatch_assert_queue_fail` → SIGTRAP) when invoked off the main thread. The 33 ms throttle at `dictation.rs:417, 426-432` only limits frequency — every individual call is at the same thread-safety risk.

  The same risk applies to QA's level_handler at `coordinator.rs:2282-2309`, which also calls `emit_capsule` directly from the cpal callback (line 2301-2308).

  Severity: high. SIGTRAP would crash the app on long recordings; less catastrophic outcomes are stuttering audio (the audio callback misses its deadline waiting for AppKit) and `kAudioUnitErr_TooManyFramesToProcess`. PR #389 needs to land or be cherry-picked before this beta is shipped.

- OK `coordinator.rs:3726` — `app.emit_to("capsule", "capsule:state", payload)` stays on the calling thread; Tauri's event bus is internally thread-safe. No change needed regardless of PR #389.
- OK `coordinator.rs:3739-3765` — `maybe_position_capsule_bottom_center` is the OS-level call inside the `if let Some(window)` block; it would be moved into the same `run_on_main_thread` closure once PR #389 lands.

### P5 — App shutdown

- OK `lib.rs:347-355` (RunEvent::Exit) — calls `stop_hotkey_listener` (Mac CGEventTap path, safe to invoke any thread per `hotkey.rs:344-355`), plus the 5 `global-hotkey`-backed `stop_*_listener`s that all marshal via `app.run_on_main_thread` (`coordinator.rs:344-358, 1313-1347`).
- OK `lib.rs:348` — `TRAY_MICROPHONE_WATCHER_STOPPING.store(true, Relaxed)` correctly signals the watcher loop spawned at `lib.rs:540-548`.
- ⚠️ `coordinator.rs:344-358, 1313-1347` — `app.run_on_main_thread` is fire-and-forget; the queued `Drop` may not run before the process exits. In practice this is fine (process exit reaps everything), but if Tauri's main-loop teardown beats the queued closure, Carbon `RemoveEventHotKey` is skipped. Same model as the pre-existing PR #169 fix for `qa_hotkey`, so flagging as an inherited limitation, not a regression.

- 🚩 **`coordinator.rs:2313` — `acquire_recording_mute(inner, "qa");` is missing `.await`.**
  - PR #391 (`6171df61`) made `acquire_recording_mute` `async fn` (`coordinator/resources.rs:122`) and updated the dictation call site to `.await` (`coordinator/dictation.rs:451`). The QA call site was missed.
  - Effect: the function returns an `impl Future` that is dropped on the next line. `spawn_blocking` is never scheduled, so `mute.holders` doesn't increment, the system audio mute is never engaged for QA, and the `[audio-mute] acquired by qa` log is never written. The corresponding `release_recording_mute(inner, "qa")` calls (e.g. `resources.rs:194`, `coordinator.rs:2324`) decrement holders that were never incremented (early `return` at `resources.rs:174` because `holders == 0`).
  - Compiler confirms it: `cargo check --manifest-path openless-all/app/src-tauri/Cargo.toml` emits

    ```
    warning: unused implementer of `futures_util::Future` that must be used
        --> src/coordinator.rs:2313:5
       2313 |     acquire_recording_mute(inner, "qa");
       |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
       = note: futures do nothing unless you `.await` or poll them
    ```

  - User-visible consequence: when the user opted into "Mute system output during recording" and triggers QA via Option, system audio is NOT muted (e.g. a YouTube playback continues). Dictation behaves correctly. Fix: insert `.await` on the call.

- OK `coordinator/resources.rs:184-188` — `release_recording_mute` falls back to synchronous `work()` when no tokio handle is present, so the recorder error monitor (a plain `std::thread::spawn`) can release safely. Drop of `AudioMuteGuard` shells out to `osascript` / `wpctl`, blocking on a std thread is OK.

## Cross-PR composition risks

1. **PR #391 + #389 incomplete merge.** The current `400097ad` contains PR #391 (which added the async hygiene the rest of the audit fixes assume) but is missing PR #389. Net result: the audio-thread → AppKit risk PR #389 fixes is still present *and* the QA codepath has a half-applied PR #391 (missing `.await` at `coordinator.rs:2313`). Both must land before this commit becomes a release candidate.
2. **PR #387 + Processing branch interaction.** PR #387 introduced `state.focus_target = None` inside `finish_cancel_session_state`, but `cancel_session` deliberately skips that helper for `Processing` (`dictation.rs:1171-1173`) so the scheduled `end_session` can drive its own teardown. `end_session`'s cancelled-after-ASR exit (`dictation.rs:845-849`) was not updated to clear `focus_target`. The contract "clear focus_target on cancel regardless of phase" is therefore violated for cancel-during-Processing. Fix can be either (a) clear `focus_target` in the cancel-after-ASR branch of `end_session`, or (b) move the clear into `cancel_session` even for Processing (does not interfere with `end_session`'s own writes).
3. **PR #390 + multi-bridge `hotkey_trigger_held`.** The dedup atomic is process-global. With both `hotkey` and `combo_hotkey` monitors running, the legacy modifier-only adapter and the custom-combo adapter share `inner.hotkey_trigger_held`. Currently exclusive (custom-combo only runs when trigger == Custom — see `coordinator.rs:413-426`), so no cross-contamination, but anything that allows them to coexist would corrupt the dedup. ℹ️ informational.
4. **PR #392 (passive flag) is dormant**, as the prompt indicates. No interaction risk; calling out only that supervisor loops gracefully ignore `shutdown=false` so future RunEvent::Exit hookup is safe to land in a follow-up PR.

## Manual-verification checklist for the user

After cherry-picking PR #389 + fixing the missing `.await` in P5, verify on a running build:

**P4 — capsule main-thread (PR #389 confirmation)**
- [ ] macOS arm64 build, dev profile, run a 3-minute continuous toggle dictation. App must not crash with SIGTRAP / `dispatch_assert_queue_fail`. Tail `~/Library/Logs/OpenLess/openless.log` while recording.
- [ ] On macOS, capsule still appears once the first PCM frame is captured (50–200 ms after Recorder::start) and disappears 1.5 s after Done/Cancelled/Error.
- [ ] On Windows, no SendMessage deadlock against the GUI thread during recording start/stop (capsule transitions complete within ~50 ms of phase changes).

**P5 — QA mute fix**
- [ ] Set `prefs.muteDuringRecording = true` (Settings → Recording).
- [ ] Play YouTube in Safari/Chrome.
- [ ] Open QA panel via `Cmd+Shift+;`. Press Right Option to start QA recording. Audio playback **must** mute. Log line `[audio-mute] acquired by qa; holders=1` must appear in `openless.log`. (Without the fix, no log line + audio keeps playing.)
- [ ] Release Right Option. Audio playback resumes; log line `[audio-mute] released by qa; holders=0` + `system output mute restored after recording` must appear.

**P3 — focus_target on Processing-cancel (PR #387 completeness)**
- [ ] Start dictation, speak briefly, release hotkey to enter Processing. While ASR is awaiting result (within ~1 s window), press Esc to trigger `cancel_dictation`. End_session bails at `cancelled` check.
- [ ] Inspect debug logs / state dump (or add a temporary log at `dictation.rs:849`): `state.focus_target` should be `None`. With current `400097ad` it remains `Some(...)`.
- [ ] Start a fresh dictation in a different window. Insertion should still target the new window — this works today via `begin_session_state` overwrite (`coordinator_state.rs:80`), so the bug is silent until something else reads stale `focus_target` between the cancel and the next begin.

**P2 — pressed/released routing (PR #390 confirmation)**
- [ ] Open QA panel (`Cmd+Shift+;`). Confirm Option starts QA recording.
- [ ] Close QA panel. Start a normal dictation (Option). While dictation is running, open QA panel via `Cmd+Shift+;` (panel becomes visible while dictation_active=true). Press and release Option once — dictation must end normally (insert text), QA must NOT capture this Option edge.
- [ ] Hold mode: Set HotkeyMode::Hold. Hold Option for 2 s, release. End_session must trigger on release (not on press).
- [ ] Toggle mode: Set HotkeyMode::Toggle. Tap Option twice rapidly during Starting phase (within the 50–200 ms cpal init window) — verify `request_stop_during_starting` queues then end_session fires on the Listening transition (search log for `applying pending_stop edge → end_session immediately`).

**P1 — listener teardown sanity**
- [ ] Quit the app via tray menu. `openless.log` should show ordered `stop_*_listener` calls; no panic / SIGTRAP at exit.
- [ ] Relaunch and grant Accessibility (after prior reset). Confirm `[hotkey] CGEventTap 已启动` returns within 3 s; first hotkey press still works without app restart.

If any of the P4/P5 checks fail, this beta is **not** build-quality.
