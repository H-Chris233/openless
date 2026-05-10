# Audit Validation — 2026-05-10

Validation of the 21 audit items against the current source tree at
`openless-all/app/src-tauri/src/`. Every CONFIRMED finding cites the
exact file and line range read from the working copy on 2026-05-10.

## Summary

| ID | Severity | Status | One-line |
|----|----------|--------|----------|
| 2.2.1 | — | CONFIRMED | TS `UserPreferences` interface and `mockSettings` both miss `updateChannel` |
| 3.1.1 | 严重 | CONFIRMED | `MacHotkeyAdapter` does not override `HotkeyAdapter::shutdown`; `CFRunLoopRun` runs forever |
| 3.1.2 | 中 | CONFIRMED | `hotkey_supervisor_loop` is `loop { ... sleep(3s) }` with no exit signal |
| 3.1.3 | 中 | CONFIRMED | `start_listener_thread` spawns the listener and drops the `JoinHandle` |
| 3.2.1 | 高 | FALSE_POSITIVE | Channel is `std::sync::mpsc::channel()` (unbounded async); `tx.send` does not block |
| 3.2.2 | 高 | CONFIRMED | `emit_capsule` calls `window.show()/hide()` from the cpal `process_callback` thread |
| 3.2.3 | 中 | CONFIRMED | `inner.inserter.insert(...)` runs sync `arboard`+`enigo` from async `end_session` |
| 3.2.4 | 中 | CONFIRMED | `AudioMuteGuard::activate` shells out to `osascript` / `wpctl` / `pactl` synchronously |
| 3.2.5 | 低 | CONFIRMED (Linux only) | `probe_input_stream` calls `thread::sleep(120ms)` from the async permission gate |
| 3.3.1 | 高 | CONFIRMED | `handle_pressed_edge` routes to QA when `panel_visible=true` regardless of dictation phase |
| 3.3.2 | 中 | PARTIAL | Two bridge loops both touch `state` for the same modifier event; non-fatal contention, no integrity bug |
| 3.3.3 | 中 | FALSE_POSITIVE | Cancelled doesn't reset coordinator latch, but the OS-side `Shared::trigger_held` already gates auto-repeat |
| 3.3.4 | 中 | CONFIRMED | `open_qa_panel` always emits `CapsuleState::Idle`, clobbering any in-flight dictation capsule |
| 3.3.5 | 低 | CONFIRMED | `finish_cancel_session_state` skips `focus_target = None` when `phase == Processing` |
| 3.3.6 | 低 | FALSE_POSITIVE | `take_current_prepared_windows_ime_session_for_restore` removes the slot on first call; second call is a true no-op |
| 3.4.1 | 中 (advisory) | ADVISORY_ONLY | `Inner` carries 16 `Mutex` + 4 `AtomicBool` fields (20 concurrent fields) |
| 3.4.2 | 中 (advisory) | ADVISORY_ONLY | 66 of 67 `Ordering` usages in coordinator/hotkey are `SeqCst` |
| 3.4.3 | 低 (advisory) | ADVISORY_ONLY | ~102 `unsafe`/`unsafe fn`/`unsafe impl`/`unsafe extern` sites; many lack SAFETY comments |
| 3.4.4 | 低 | CONFIRMED | `start_dispatcher` in `global_hotkey_runtime.rs` is `loop {}` with no exit |
| 20 (NEW) | — | FALSE_POSITIVE | `read_or_default` already falls back to `UserPreferences::default()` on decode failure; `expect()` only fires on filesystem errors |
| 2.3.3 | — | CONFIRMED (no action) | All four backend events (`capsule:state`, `qa:state`, `qa:level`, `vocab:updated`) have matching frontend listeners |

**Tally**: 11 CONFIRMED · 4 FALSE_POSITIVE · 1 PARTIAL · 3 ADVISORY_ONLY · 1 CONFIRMED-no-action · 1 CONFIRMED-Linux-only

## Recommended PR groupings

Group by file to minimize merge conflict risk. Suggested order:

1. **PR A — `hotkey.rs` lifecycle** (3.1.1, 3.1.3): add `MacHotkeyAdapter::shutdown` (post a synthetic `CFRunLoopStop`/`CFRunLoopWakeUp` from `Drop`) and store the listener `JoinHandle` so panics surface. Same file, same review.
2. **PR B — TS type alignment** (2.2.1): add `updateChannel: UpdateChannel` to `src/lib/types.ts` and `mockSettings` in `src/lib/ipc.ts`. One file pair, trivial.
3. **PR C — coordinator hotkey supervisor exits** (3.1.2, 3.4.4): add an `AtomicBool` shutdown flag to `hotkey_supervisor_loop` and `global_hotkey_runtime::start_dispatcher`. Same module concern, no overlap with PR A.
4. **PR D — async hygiene** (3.2.3, 3.2.4, 3.2.5): wrap `inserter.insert`, `AudioMuteGuard::activate`, and `probe_input_stream` in `tokio::task::spawn_blocking`. Touches `coordinator/dictation.rs`, `coordinator/resources.rs`, and `coordinator.rs` — coordinate with PR E to avoid shared-line conflicts.
5. **PR E — QA / dictation routing race** (3.3.1, 3.3.4): make `handle_pressed_edge` consult dictation phase before routing to QA, and skip the `Idle` capsule clobber in `open_qa_panel` when dictation is active. Same file (`coordinator/qa.rs` + `coordinator/dictation.rs`).
6. **PR F — capsule UI thread marshaling** (3.2.2): bounce `window.show()/hide()` through `app.run_on_main_thread`; emit-only path can stay (Tauri marshals events internally). Touches `coordinator.rs::emit_capsule`. Independent of PR E.
7. **PR G — focus_target leak fix** (3.3.5): in `finish_cancel_session_state`, also clear `focus_target` when the cancelled phase is `Processing`. Pure `coordinator_state.rs` edit.

Advisory items (3.4.1 / 3.4.2 / 3.4.3) need no PR; they are tracked here for future hardening.

## Detail per item

### 2.2.1 — `updateChannel` missing in TS types
**Status**: CONFIRMED
**Files**: `openless-all/app/src-tauri/src/types.rs:216-219`, `openless-all/app/src/lib/types.ts:118-177`, `openless-all/app/src/lib/ipc.ts:46-79`

**Evidence (Rust source of truth)**:
```rust
// types.rs:216-219
/// Auto-update 渠道偏好。stable = 跟正式版（默认）；beta = Settings 里多
/// 一个手动下载 Beta 的入口。不影响 plugin-updater 的自动检查路径。
#[serde(default)]
pub update_channel: UpdateChannel,
```

**Evidence (TS gap)**: `UserPreferences` ends at `startMinimized: boolean;` (line 176). No `updateChannel` field. `mockSettings` (ipc.ts:46-79) ends at `startMinimized: false,`. No `updateChannel` key.

**Notes**: Channel state today is read/written via separate `getUpdateChannel` / `setUpdateChannel` IPC commands (`ipc.ts:170-176`), so `getSettings()` still works — the TS shape is just lying about what the Rust backend actually serializes. Setting via `setSettings(prefs)` round-trips through Rust's `UserPreferencesWire` which `#[serde(default)]`-fills the field, so currently no data corruption, but the type is incorrect and any consumer that destructures `UserPreferences` will silently miss the field. Trivial fix.

---

### 3.1.1 — `MacHotkeyAdapter::shutdown` empty [严重]
**Status**: CONFIRMED
**Files**: `openless-all/app/src-tauri/src/hotkey.rs:111` (default), `:301-325` (`MacHotkeyAdapter` impl), `:419-458` (`run_listen_loop`), contrast `:769-776` (Windows shutdown).

**Evidence (default)**:
```rust
// hotkey.rs:102-112
pub trait HotkeyAdapter: Send + Sync {
    fn kind(&self) -> HotkeyAdapterKind;
    fn update_binding(&self, binding: HotkeyBinding);
    fn update_modifier_shortcuts(...);
    fn reset_held_state(&self);
    fn shutdown(&self) {}
}
```

**Evidence (mac adapter impl is silent on `shutdown`)**:
```rust
// hotkey.rs:305-325
impl HotkeyAdapter for MacHotkeyAdapter {
    fn kind(&self) -> HotkeyAdapterKind { ... }
    fn update_binding(&self, binding: HotkeyBinding) { ... }
    fn update_modifier_shortcuts(...) { ... }
    fn reset_held_state(&self) { reset_shared_held_state(&self.shared); }
    // <-- no shutdown override
}
```

**Evidence (no exit path)**:
```rust
// hotkey.rs:454-457
log::info!("[hotkey] CGEventTap 已启动");
let _ = status_tx.send(Ok(()));
CFRunLoopRun();
// CFRunLoopRun never returns absent CFRunLoopStop; the listener thread leaks.
```

**Notes**: `Drop for HotkeyMonitor` does call `self.adapter.shutdown()` (line 170-174), but Mac falls through to the empty default. On every preference-driven monitor swap the old `CFRunLoop` thread + tap leak. Visible on macOS as a steady leak of background threads on long-running sessions that change hotkey bindings.

**Fix sketch**: Store the `CfRunLoopRef` returned by `CFRunLoopGetCurrent()` (currently captured in `run_listen_loop` only) in the `MacHotkeyAdapter`, plus the `CfMachPortRef`. On `shutdown`, call `CGEventTapEnable(tap, false)` then `CFRunLoopStop(rl)`. Both are FFI-safe to call from any thread. Mirror the Windows pattern (`PostThreadMessageW(thread_id, WM_QUIT)`). Storing those refs needs `CallbackContext` exposed to the adapter, easiest via shared `parking_lot::Mutex`.

---

### 3.1.2 — `hotkey_supervisor_loop` no-exit
**Status**: CONFIRMED
**Files**: `openless-all/app/src-tauri/src/coordinator.rs:766-833`

**Evidence**:
```rust
// coordinator.rs:766-833
fn hotkey_supervisor_loop(inner: Arc<Inner>) {
    let mut attempts: u32 = 0;
    let capability = HotkeyMonitor::capability();
    loop {
        let prefs = inner.prefs.get();
        if inner.hotkey.lock().is_some() { return; }
        // ... try start, on failure:
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}
```

**Notes**: The only successful exit is when the hotkey is already installed (line 772-774). On error the supervisor keeps spinning. There is no `AtomicBool` / `Sender<()>` shutdown signal exposed for app shutdown. Same pattern repeats in `qa_hotkey_supervisor_loop`, `combo_hotkey_supervisor_loop`, `translation_hotkey_supervisor_loop`, `action_hotkey_supervisor_loop`. Worth a single shared shutdown flag.

---

### 3.1.3 — `start_listener_thread` drops `JoinHandle`
**Status**: CONFIRMED
**Files**: `openless-all/app/src-tauri/src/hotkey.rs:196-229`

**Evidence**:
```rust
// hotkey.rs:218-228
let (status_tx, status_rx) = mpsc::channel::<Result<T, HotkeyInstallError>>();
std::thread::Builder::new()
    .name(thread_name.into())
    .spawn(move || run_listen_loop(thread_shared, tx, status_tx))
    .map_err(|e| install_error("spawn_failed", format!("hotkey 线程启动失败: {e}")))?;

match status_rx.recv_timeout(Duration::from_secs(3)) { ... }
```

**Notes**: The `Result<JoinHandle, _>` from `spawn(...)` is `?`'d for the spawn error, but the `JoinHandle` itself is silently dropped (the spawn return value isn't bound). `ListenerThread<T>` only stores `shared` and a single `startup` value. If the listener panics (e.g. `parking_lot::RwLock` poisoning, FFI bug), there is no path for the supervisor to learn about it — the channel just stops receiving. Storing the handle and using `JoinHandle::is_finished()` (Rust 1.61+) or pairing with a "thread alive" `AtomicBool` would let the supervisor restart the listener on panic.

---

### 3.2.1 — Blocking `tx.send` in event-tap callback
**Status**: FALSE_POSITIVE
**Files**: `openless-all/app/src-tauri/src/hotkey.rs:183-187`, `:218`, `coordinator.rs:650`, `:781`

**Evidence**:
```rust
// hotkey.rs:183-187
fn send_or_log(tx: &Sender<HotkeyEvent>, evt: HotkeyEvent) {
    if let Err(e) = tx.send(evt) {
        log::warn!("[hotkey] 事件发送失败: {e}");
    }
}
```

```rust
// coordinator.rs:650 (and :781)
let (tx, rx) = mpsc::channel::<HotkeyEvent>();
```

**Notes**: `std::sync::mpsc::channel()` is the **unbounded asynchronous** variant — `Sender::send` only blocks-and-fails when the receiver has been dropped, in which case it returns `Err(SendError)` immediately. There is no "rendezvous" backpressure. The only way `tx.send` would block long enough to trip `kCGEventTapDisabledByTimeout` is if std mpsc internally allocated under contention, which would be milliseconds at worst, not the seconds-scale macOS uses for the tap-disabled timeout. The audit conflated `mpsc::channel()` with `mpsc::sync_channel(0)` (rendezvous). No fix needed.

---

### 3.2.2 — `emit_capsule` runs `show()/hide()` on cpal callback thread
**Status**: CONFIRMED
**Files**: `openless-all/app/src-tauri/src/coordinator/dictation.rs:336-365`, `coordinator.rs:3617-3660`, `recorder.rs:458-490`

**Evidence (call site is the audio callback)**:
```rust
// coordinator/dictation.rs:336-365
let level_handler: Arc<dyn Fn(f32) + Send + Sync> = Arc::new(move |level| {
    // ...phase guard, throttle to ~30 Hz...
    emit_capsule(
        &inner_for_level,
        CapsuleState::Recording,
        level,
        elapsed,
        None,
        None,
    );
});
// ...
match Recorder::start(microphone_device_name, consumer, level_handler) { ... }
```

**Evidence (callback thread is cpal's `process_callback`)**:
```rust
// recorder.rs:458-482 (process_callback)
fn process_callback(...) {
    // ...resampling, RMS computation...
    level_handler(level);  // synchronously invoked from cpal audio thread
}
```

**Evidence (`emit_capsule` directly touches the window)**:
```rust
// coordinator.rs:3637-3656
if let Some(window) = app.get_webview_window("capsule") {
    let visible = !matches!(state, CapsuleState::Idle);
    maybe_position_capsule_bottom_center(inner, &window, payload.translation);
    if show_capsule && visible {
        if !show_capsule_window_no_activate(&app, &window) {
            let _ = window.show();
        }
        // ...
    } else {
        hide_capsule_window_if_present();
        let _ = window.hide();
    }
}
let _ = app.emit_to("capsule", "capsule:state", payload);
```

**Notes**: `app.emit_to` is fine — Tauri's event bus is thread-safe and marshals to the JS runtime internally. The risk is `window.show() / window.hide()` and the position helper, all of which call NSWindow / HWND APIs that expect the main thread. On macOS this can stall the audio callback (NSWindow ops grab the AppKit run loop), risking `kAudioUnitErr_TooManyFramesToProcess` if the callback misses its deadline. The throttle to ~30 Hz mitigates frequency but doesn't change the per-call risk. Worth bouncing the window-touching half through `app.run_on_main_thread`.

**Fix sketch**: Split `emit_capsule` into `emit_capsule_event` (just `app.emit_to`, safe everywhere) and `apply_capsule_window_state` (called inside `app.run_on_main_thread` or only from already-main paths). The level-handler path only needs the event; window show/hide already happens in begin/end/cancel which run on the tokio runtime where `run_on_main_thread` is cheap.

---

### 3.2.3 — Sync inserter from async `end_session`
**Status**: CONFIRMED
**Files**: `openless-all/app/src-tauri/src/coordinator/dictation.rs:900-913`, `insertion.rs:42-56`, `:74-95`, `:136-149`

**Evidence**:
```rust
// coordinator/dictation.rs:900-913
#[cfg(not(target_os = "windows"))]
{
    inner.inserter.insert(&polished, restore_clipboard)
}
// ...
} else if allow_non_tsf_insertion_fallback {
    inner.inserter.copy_fallback(&polished)
}
```

```rust
// insertion.rs:42-46 (non-macOS impl)
pub fn insert(&self, text: &str, restore_clipboard_after_paste: bool) -> InsertStatus {
    if text.is_empty() { return InsertStatus::CopiedFallback; }
    insert_with_clipboard_restore(text, restore_clipboard_after_paste)
}
```

**Notes**: `end_session` is `async`. On Linux (and on macOS too — `simulate_paste()` is FFI-light but still sync), `insert` calls `arboard::Clipboard::new()` which can block on X11/wayland for tens of ms, then `enigo` keystroke synthesis which is also sync. Blocking the tokio worker for 50–200 ms isn't catastrophic but contributes to latency under load. macOS path uses `CGEventPost` via FFI — fast, non-blocking in practice; mostly a Linux/Windows concern.

**Fix sketch**: Wrap the platform-specific `inserter.insert` / `inserter.copy_fallback` in `tokio::task::spawn_blocking(move || ...).await.unwrap_or(InsertStatus::Failed)`. `TextInserter` is `Sync`, so the move only needs `&self` cloned via `Arc` (already in `Inner`).

---

### 3.2.4 — `AudioMuteGuard` shells out synchronously
**Status**: CONFIRMED
**Files**: `openless-all/app/src-tauri/src/audio_mute.rs:127-152` (macOS), `:202-249` (Linux), `coordinator/resources.rs:112-131`, `coordinator/dictation.rs:369`

**Evidence (macOS `osascript`)**:
```rust
// audio_mute.rs:143-151
let output = Command::new("osascript")
    .args(["-e", script])
    .output()
    .map_err(|e| format!("set output mute failed: {e}"))?;
```

**Evidence (Linux `wpctl`/`pactl` — same shape)**:
```rust
// audio_mute.rs:215-223
let output = Command::new("wpctl")
    .args(["set-mute", "@DEFAULT_AUDIO_SINK@", value])
    .output()
    .map_err(|e| format!("wpctl set-mute failed: {e}"))?;
```

**Evidence (called from async `begin_session`)**:
```rust
// coordinator/dictation.rs:369
acquire_recording_mute(inner, "dictation");
match Recorder::start(microphone_device_name, consumer, level_handler) { ... }
```

```rust
// coordinator/resources.rs:117-127
if mute.holders == 0 {
    match crate::audio_mute::AudioMuteGuard::activate() {
        Ok(guard) => { mute.guard = Some(guard); ... }
        Err(err) => { ... }
    }
}
```

**Notes**: `osascript` typically takes 100–300 ms to spawn + execute on macOS (AppleScript runtime startup). On a hot-key press → begin_session, this delays the recording start by exactly that amount, on the tokio worker thread. Windows path uses native COM (`IAudioEndpointVolume::SetMute`) which is fast and OK. Linux `wpctl`/`pactl` is similar to macOS osascript in cost.

**Fix sketch**: Wrap `AudioMuteGuard::activate()` in `tokio::task::spawn_blocking`. Since `acquire_recording_mute` itself is sync and called from `begin_session` (async), the cleanest path is making `acquire_recording_mute` async and `.await`-ing the spawn_blocking. Drop path (`PlatformMuteGuard::restore`) also runs `osascript` and is currently called from `Drop` in `release_recording_mute`; moving that to a detached `tokio::spawn_blocking` is sufficient (release path doesn't need to await).

---

### 3.2.5 — `thread::sleep(120ms)` in async permission probe
**Status**: CONFIRMED (Linux/non-macOS path only)
**Files**: `openless-all/app/src-tauri/src/permissions.rs:323-357`, `coordinator/dictation.rs:137`, `coordinator.rs:1732-1763`

**Evidence**:
```rust
// permissions.rs:343-356
let stream = match sample_format {
    SampleFormat::F32 => build_probe!(f32),
    // ...
}?;
stream.play().map_err(|e| e.to_string())?;
std::thread::sleep(Duration::from_millis(120));
drop(stream);
Ok(())
```

**Evidence (called from async)**:
```rust
// coordinator/dictation.rs:137
if let Err(message) = ensure_microphone_permission(inner) { ... }
```

```rust
// coordinator.rs:1732-1763
fn ensure_microphone_permission(_inner: &Arc<Inner>) -> Result<(), String> {
    #[cfg(target_os = "windows")] { ... return Ok(()); }   // Windows skips probe
    let status = permissions::check_microphone();
    // ...
}
```

**Notes**: On macOS `check_microphone` uses `AVAudioApplication` / `AVCaptureDevice` and never reaches `probe_input_stream` (that helper is in the `cfg(not(target_os = "macos"))` module). So this 120 ms blocking sleep only fires on **Linux** (and any other non-macOS, non-Windows path) when probing mic permission. On Linux dictation, every `begin_session` blocks the tokio worker for 120 ms before the recorder even starts.

**Fix sketch**: `tokio::time::sleep(Duration::from_millis(120)).await` is the correct call but requires turning `probe_input_stream` async. Alternatively keep it sync and wrap the whole `check_microphone()` non-macOS path in `tokio::task::spawn_blocking`. The latter is mechanically simpler.

---

### 3.3.1 — Pressed edge routes to QA without checking dictation phase
**Status**: CONFIRMED
**Files**: `openless-all/app/src-tauri/src/coordinator/dictation.rs:11-22`, `coordinator/qa.rs:64-77`

**Evidence**:
```rust
// coordinator/dictation.rs:11-22
pub(super) async fn handle_pressed_edge(inner: &Arc<Inner>) {
    let was_held = inner.hotkey_trigger_held.swap(true, Ordering::SeqCst);
    if !was_held {
        // 路由：QA 浮窗可见时，rightOption 边沿走 QA；否则走主听写。详见 issue #118 v2。
        let panel_visible = inner.qa_state.lock().panel_visible;
        if panel_visible {
            handle_qa_option_edge(inner).await;
        } else {
            handle_pressed(inner).await;
        }
    }
}
```

**Evidence (QA path doesn't stop dictation)**:
```rust
// coordinator/qa.rs:64-77
pub(super) async fn handle_qa_option_edge(inner: &Arc<Inner>) {
    let phase = inner.qa_state.lock().phase;
    log::info!("[coord] QA option edge (phase={phase:?})");
    match phase {
        QaPhase::Idle => { let _ = begin_qa_session(inner).await; }
        QaPhase::Recording => { let _ = end_qa_session(inner).await; }
        QaPhase::Processing => {}
    }
}
```

**Notes**: `panel_visible` flips true via `open_qa_panel`, which is triggered by the QA hotkey (`Cmd+Shift+;` by default). If the user opens the QA panel mid-dictation (dictation `phase = Listening`, mic open, ASR session live), the next dictation-hotkey press routes into `begin_qa_session`. `begin_qa_session` will call `Recorder::start` again on the same mic device. cpal will reject the second `build_input_stream` on most platforms, but on Linux/PipeWire it sometimes succeeds and you end up with two concurrent capture streams. Even where it fails, the dictation session's recorder is still running and the user has no UX path to stop it from the QA panel.

**Fix sketch**: In `handle_pressed_edge`, check `inner.state.lock().phase`. If `Listening` or `Starting`, route to `handle_pressed` (which is the dictation toggle path) regardless of `panel_visible`, and either close the QA panel or refuse to open it while dictation is active. Decision belongs to product, but the *technical* race is real.

---

### 3.3.2 — Dual TranslationModifier handlers
**Status**: PARTIAL
**Files**: `openless-all/app/src-tauri/src/coordinator.rs:1130-1139`, `:1376-1384`, `:1402-1410`

**Evidence**:
```rust
// coordinator.rs:1130-1139 (translation_hotkey_bridge_loop, runs on its own thread)
fn translation_hotkey_bridge_loop(inner: Arc<Inner>, rx: mpsc::Receiver<ComboHotkeyEvent>) {
    while let Ok(evt) = rx.recv() {
        if inner.shortcut_recording_active.load(Ordering::SeqCst) { continue; }
        if matches!(evt, ComboHotkeyEvent::Pressed) {
            mark_translation_modifier_seen(&inner);
        }
    }
}

// coordinator.rs:1402-1410 (hotkey_bridge_loop, separate thread)
HotkeyEvent::TranslationModifierPressed => {
    let translation_hotkey = inner_cloned.prefs.get().translation_hotkey;
    if is_builtin_translation_shift(&translation_hotkey)
        || crate::shortcut_binding::legacy_modifier_trigger(&translation_hotkey)
            .is_some()
    {
        mark_translation_modifier_seen(&inner_cloned);
    }
}

// coordinator.rs:1376-1384
fn mark_translation_modifier_seen(inner: &Arc<Inner>) {
    let phase = inner.state.lock().phase;
    if matches!(phase, SessionPhase::Starting | SessionPhase::Listening) {
        inner.translation_modifier_seen.store(true, Ordering::SeqCst);
    }
}
```

**Notes**: Both bridge loops run on independent `std::thread`s and both ultimately call `mark_translation_modifier_seen`, which locks `inner.state`. They never run *racing on integrity* — they both write the same `AtomicBool::store(true)`, idempotent. The audit's framing of "Both lock `inner.state` independently" is technically true but not a bug — `state` is a `Mutex`, only one acquires at a time, and both write the same flag. Worst case is one log-line of `[coord] translation modifier seen during ...` printed twice for one Shift press. Not worth a code change unless 3.3.1's fix touches the same code.

---

### 3.3.3 — Cancelled doesn't reset `hotkey_trigger_held`
**Status**: FALSE_POSITIVE
**Files**: `openless-all/app/src-tauri/src/coordinator.rs:1399-1401`, `hotkey.rs:530-538`, `coordinator/dictation.rs:11-22`

**Evidence**:
```rust
// coordinator.rs:1399-1401
HotkeyEvent::Cancelled => {
    cancel_session(&inner_cloned);
}
```

**Why it doesn't actually wedge**: `HotkeyEvent::Cancelled` is emitted by the OS-side hotkey monitor only when the user presses **Esc** (`hotkey.rs:565-570` for macOS, `:867-871` for Windows), not when the dictation trigger key is released. The dictation trigger's "is currently held" state lives in `Shared::trigger_held` inside the platform monitor (`hotkey.rs:117`). That atomic gates re-emission of Pressed via the `is_active && !was_held` check (`hotkey.rs:530-538`). So even if `Inner::hotkey_trigger_held` stays `true` in the coordinator after Esc, the next Pressed edge from the OS will only fire when the user releases and re-presses the trigger key — and the OS path will set `was_held=false` again before sending Pressed. The coordinator's `hotkey_trigger_held` swap on the next Pressed will return `false` (because `handle_released_edge` has run between the previous press and this one, OR the user never released, in which case no new Pressed is queued).

The audit confused two layers: the OS-edge dedupe in `hotkey.rs::Shared::trigger_held` (which is the gating thing) and the coordinator's `Inner::hotkey_trigger_held` (which is just a bookkeeping latch tied to Pressed/Released edges that already came in). Cancelled doesn't change either's correctness.

**Notes**: There's a *cosmetic* asymmetry — after Esc, `Inner::hotkey_trigger_held=true` until the user releases the trigger. If the user keeps holding past the Esc, `Released` fires later and resets it. Defensive cleanup would be to also reset on Cancelled, but it doesn't fix any user-visible bug.

---

### 3.3.4 — `open_qa_panel` clobbers Done capsule
**Status**: CONFIRMED
**Files**: `openless-all/app/src-tauri/src/coordinator/qa.rs:79-104`

**Evidence**:
```rust
// coordinator/qa.rs:79-104
pub(super) fn open_qa_panel(inner: &Arc<Inner>) {
    {
        let mut state = inner.qa_state.lock();
        state.panel_visible = true;
        state.phase = QaPhase::Idle;
        // ...
    }
    // 先把胶囊清干净，避免主听写上一次 Done 状态残留的 message/insertedChars
    // 在 QA Done 阶段被 capsule UI 错误复用（"已之一粘贴这个 0" 那种）。
    emit_capsule(inner, CapsuleState::Idle, 0.0, 0, None, None);
    // ...
}
```

**Notes**: The comment shows the design intent is *intentional* — sweep stale Done state from a previous dictation. But it sweeps *any* in-flight capsule too. If the user opens the QA panel within the ~1.5 s `CAPSULE_AUTO_HIDE_DELAY_MS` window after dictation finishes, they lose the "已粘贴 N 字" toast. More importantly, if dictation is still in `Polishing` or `Inserting` phase (LLM hasn't returned yet), opening QA hides the polish-progress capsule mid-flight. The user sees their dictation "vanish" until insertion completes.

**Fix sketch**: Before calling `emit_capsule(Idle, ...)`, check `inner.state.lock().phase`. Only sweep if dictation is in `Idle`. If dictation is mid-flight, leave the capsule visible — QA panel doesn't need the capsule cleared to function. Pairs cleanly with the 3.3.1 fix (same source files).

---

### 3.3.5 — `focus_target` leaks on Processing-phase cancel
**Status**: CONFIRMED
**Files**: `openless-all/app/src-tauri/src/coordinator_state.rs:155-173`, `:347-374` (test that already proves the gap)

**Evidence**:
```rust
// coordinator_state.rs:168-173
pub(crate) fn finish_cancel_session_state(state: &mut SessionState, decision: CancelDecision) {
    if decision.phase != SessionPhase::Processing {
        state.phase = SessionPhase::Idle;
        state.focus_target = None;
    }
}
```

**Evidence (existing test acknowledges the gap)**:
```rust
// coordinator_state.rs:370-372
if matches!(initial, SessionPhase::Starting | SessionPhase::Listening) {
    assert!(state.focus_target.is_none(), "initial={initial:?}");
}
// Note: no assertion that Processing-phase cancel clears focus_target.
```

**Notes**: When cancel hits `Processing`, `finish_cancel_session_state` deliberately keeps `phase=Processing` (the comment says "交给 end_session 自己收尾"), but it also keeps `focus_target` populated. `end_session` does eventually reset state via the `proceed_to_insert=false` branch (`coordinator/dictation.rs:862-878`) which sets `phase=Idle` but doesn't touch `focus_target`. Net result: stale `focus_target` (a `Vec<PreparedWindowsImeSessionSlot>` index, basically a plain `usize`) lives on into the next `begin_session_state`, which overwrites it (`coordinator_state.rs:80`). So the leak is bounded — next session clobbers it. Real risk is only between cancel and next begin, where `restore_focus_target_if_possible(focus_target)` could pick up a stale value if anyone reads it. Code review didn't surface a reader on that interval, so this is a low-impact correctness gap, not a user-facing bug.

**Fix sketch**: In `finish_cancel_session_state`, set `state.focus_target = None` unconditionally (before the phase check). The Processing branch's existing semantic — "let `end_session` collapse to Idle" — doesn't depend on `focus_target` staying set.

---

### 3.3.6 — Double-restore of prepared Windows IME session
**Status**: FALSE_POSITIVE
**Files**: `openless-all/app/src-tauri/src/coordinator.rs:1594-1633`

**Evidence**:
```rust
// coordinator.rs:1594-1602
fn take_matching_prepared_windows_ime_session(
    slots: &mut Vec<PreparedWindowsImeSessionSlot>,
    session_id: SessionId,
) -> Option<PreparedWindowsImeSession> {
    let index = slots
        .iter()
        .position(|slot| slot.session_id == session_id)?;
    Some(slots.remove(index).prepared)
}
```

```rust
// coordinator.rs:1620-1633
fn restore_prepared_windows_ime_session(inner: &Arc<Inner>, session_id: SessionId) {
    let state = inner.state.lock();
    let prepared = {
        let mut slot = inner.prepared_windows_ime_session.lock();
        take_current_prepared_windows_ime_session_for_restore(
            &mut slot, session_id, state.session_id,
        )
    };
    if let Some(prepared) = prepared { inner.windows_ime.restore_session(prepared); }
}
```

**Notes**: First call to `restore_prepared_windows_ime_session` for a given `session_id` does `slots.remove(index)` regardless of the freshness check on `current_session_id`. The slot is gone after that. Second call's `slots.iter().position(...)` returns `None`, the `?` short-circuits, the function silently no-ops. So even if `cancel_session → end_session` (or vice versa) both invoke `restore_prepared_windows_ime_session` with the same `session_id`, the IME state is restored at most once. The audit's worry is unfounded.

---

### 3.4.1 — Inner has many lock fields
**Status**: ADVISORY_ONLY
**Files**: `openless-all/app/src-tauri/src/coordinator.rs:91-141`

**Inventory**: 16 `Mutex<...>` + 4 `AtomicBool` (excluding `Arc<...>` indirection counts) plus an `Arc<Mutex<Vec<...>>>` for the windows IME slot vector. Specifically:

| Field | Type |
|---|---|
| `app` | `Mutex<Option<AppHandle>>` |
| `state` | `Mutex<SessionState>` |
| `asr` | `Mutex<Option<SessionResource<ActiveAsr>>>` |
| `recorder` | `Mutex<Option<SessionResource<Recorder>>>` |
| `recording_mute` | `Mutex<SharedRecordingMuteState>` |
| `hotkey` | `Mutex<Option<HotkeyMonitor>>` |
| `hotkey_status` | `Mutex<HotkeyStatus>` |
| `hotkey_trigger_held` | `AtomicBool` |
| `shortcut_recording_active` | `AtomicBool` |
| `combo_hotkey` | `Mutex<Option<ComboHotkeyMonitor>>` |
| `translation_hotkey` | `Mutex<Option<ComboHotkeyMonitor>>` |
| `switch_style_hotkey` | `Mutex<Option<ComboHotkeyMonitor>>` |
| `open_app_hotkey` | `Mutex<Option<ComboHotkeyMonitor>>` |
| `translation_modifier_seen` | `AtomicBool` |
| `qa_hotkey` | `Mutex<Option<QaHotkeyMonitor>>` |
| `qa_state` | `Mutex<QaSessionState>` |
| `capsule_layout` | `Mutex<Option<CapsuleLayoutState>>` |
| `qa_asr` | `Mutex<Option<Arc<VolcengineStreamingASR>>>` |
| `qa_recorder` | `Mutex<Option<Recorder>>` |
| `qa_stream_cancelled` | `Arc<AtomicBool>` (one of two AtomicBools-in-Arc) |
| `prepared_windows_ime_session` (windows-only) | `Arc<Mutex<Vec<PreparedWindowsImeSessionSlot>>>` |

No deadlock pattern was found in the read paths — most call sites take one lock at a time. `mark_translation_modifier_seen` and `cancel_session` both touch `inner.state`, but in disjoint critical sections. Documenting only.

---

### 3.4.2 — Heavy `Ordering::SeqCst` use
**Status**: ADVISORY_ONLY
**Files**: `coordinator.rs`, `coordinator/*.rs`, `hotkey.rs`

**Evidence**: `grep -rn "Ordering::SeqCst" coordinator.rs coordinator/ hotkey.rs | wc -l` → 66. Total `Ordering::*` uses in those files: 67 (one `Relaxed` in `recorder.rs::process_callback`, 66 SeqCst).

**Notes**: Most uses are simple set/load on independent `AtomicBool`s where `Ordering::Relaxed` would suffice. A few that gate cross-thread visibility (`hotkey_trigger_held` swap in `handle_pressed_edge` synchronizing with the audio thread reading session state) might justify Acquire/Release. `SeqCst` is correct everywhere — just over-strong. Not a bug.

---

### 3.4.3 — Many `unsafe` blocks, audit SAFETY comments
**Status**: ADVISORY_ONLY
**Files**: cross-tree (predominantly `hotkey.rs`, `insertion.rs`, `windows_ime_*.rs`, `permissions.rs`)

**Evidence**: `grep -rn "unsafe " src/ --include="*.rs" | grep -E "unsafe \{|unsafe fn|unsafe impl|unsafe extern"` → 102 sites.

**Notes**: Almost all are platform FFI (CoreFoundation/CoreGraphics on macOS, win32 on Windows, msg_send! on macOS objc2). Sample inspected (`insertion.rs::send_text` near line 340 and `post_cmd_v` near line 420) — function-level invariants are documented at module level, but inline `// SAFETY:` comments are sparse. Same for `hotkey.rs::run_listen_loop` which leaks `Box::into_raw` for FFI context — a `// SAFETY: ctx is dropped only inside the listener after CFRunLoopRun returns; reentrancy guarded by ...` comment would help. Documentation-grade improvement, no soundness bug detected.

---

### 3.4.4 — Global hotkey dispatcher loop has no exit
**Status**: CONFIRMED
**Files**: `openless-all/app/src-tauri/src/global_hotkey_runtime.rs:94-107`

**Evidence**:
```rust
// global_hotkey_runtime.rs:94-107
fn start_dispatcher(runtime: Arc<GlobalHotkeyRuntime>) {
    std::thread::Builder::new()
        .name("openless-global-hotkey-dispatch".into())
        .spawn(move || {
            let receiver = GlobalHotKeyEvent::receiver();
            loop {
                match receiver.recv_timeout(Duration::from_millis(250)) {
                    Ok(event) => runtime.dispatch(event),
                    Err(_) => continue,
                }
            }
        })
        .expect("spawn global hotkey dispatcher");
}
```

**Notes**: `GlobalHotKeyEvent::receiver()` is process-global from the upstream `global-hotkey` crate. The 250 ms timeout means the loop wakes regularly but never checks an exit flag. On app shutdown the thread leaks; harmless for a single-instance app but trips `tokio::test` and any future `Drop`-based teardown (e.g. integration tests that spin coordinator up/down).

**Fix sketch**: Pair with `Inner` shutdown flag added in 3.1.2 fix, or use a `parking_lot::RwLock<Option<...>>` "dispatcher alive" gate inside `GlobalHotkeyRuntime` that the loop reads each iteration. Same shape as the Windows hotkey thread's `WM_QUIT` plumbing.

---

### 20 (NEW) — `PreferencesStore::new().expect(...)` panics on bad prefs
**Status**: FALSE_POSITIVE
**Files**: `openless-all/app/src-tauri/src/coordinator.rs:169`, `:210`, `persistence.rs:790-811`, `:146-156`, `types.rs:368-422`

**Evidence (deserialization fallback already exists)**:
```rust
// persistence.rs:790-811
impl PreferencesStore {
    pub fn new() -> Result<Self> {
        let dir = data_dir()?;
        ensure_dir(&dir)?;
        let path = dir.join(PREFERENCES_FILE);
        let prefs = if path.exists() {
            read_or_default::<UserPreferences>(&path).unwrap_or_else(|e| {
                log::warn!(
                    "[prefs] load {} failed, using defaults: {}",
                    path.display(),
                    e
                );
                UserPreferences::default()
            })
        } else {
            UserPreferences::default()
        };
        Ok(Self { path, state: Mutex::new(prefs), })
    }
}
```

```rust
// persistence.rs:146-156 (read_or_default)
fn read_or_default<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T> {
    if !path.exists() { return Ok(T::default()); }
    let bytes = fs::read(path).with_context(|| format!("read failed: {}", path.display()))?;
    if bytes.is_empty() { return Ok(T::default()); }
    serde_json::from_slice::<T>(&bytes)
        .with_context(|| format!("decode failed: {}", path.display()))
}
```

**Why the audit was wrong**: The custom `Deserialize for UserPreferences` (types.rs:368-422) does call `default_dictation_hotkey_from_legacy(...).map_err(serde::de::Error::custom)?` which can return a serde error for `trigger == Custom` without `customComboHotkey`. That error bubbles through `serde_json::from_slice::<UserPreferences>` to `read_or_default`, which propagates it as `Result::Err`. But `PreferencesStore::new` then catches it at `.unwrap_or_else(|e| { log::warn!(...); UserPreferences::default() })`. So the `expect("preferences store init")` at coordinator.rs:169 only fires if `data_dir()?` or `ensure_dir(&dir)?` fails — i.e. the OS-level Application Support directory cannot be created/accessed, which is a legitimate fail-fast condition (no preferences-file storage, no point continuing).

The audit conflated "deserialization fails" with "PreferencesStore::new returns Err". In the current code those are different.

**Notes**: No fix needed. The "bad prefs file" case is already handled silently (log + default). If you want belt-and-braces against panic on the truly-impossible filesystem failure, you can add a final `.unwrap_or_else` that logs and returns a fully-default in-memory store, but that's defensive coding for a case where the user's machine is so broken that Application Support is unwritable.

---

### 2.3.3 — Event-name alignment between backend emit and frontend listen
**Status**: CONFIRMED (no action)
**Files**: `coordinator.rs:3659`, `coordinator/qa.rs:94-101`, `coordinator/dictation.rs:929-931`, `src/components/Capsule.tsx:293`, `src/pages/QaPanel.tsx:55,116`, `src/pages/Vocab.tsx:51`

**Notes**: Backend emits `capsule:state`, `qa:state`, `qa:level`, `vocab:updated`. Frontend listens to all four under matching names (Capsule, QaPanel, Vocab respectively). No mismatch. As stated in the audit prompt — already verified, retained here for completeness.

## Files referenced

- `openless-all/app/src-tauri/src/types.rs` (lines 57, 200-525, especially 216-219, 277-325, 368-422)
- `openless-all/app/src-tauri/src/hotkey.rs` (lines 1-250, 280-572, 698-870, 1126-1190)
- `openless-all/app/src-tauri/src/coordinator.rs` (lines 91-141, 156-313, 640-833, 837-940, 1130-1139, 1376-1419, 1582-1636, 3617-3700)
- `openless-all/app/src-tauri/src/coordinator/dictation.rs` (lines 1-160, 320-410, 810-1050)
- `openless-all/app/src-tauri/src/coordinator/qa.rs` (full file, 1-124)
- `openless-all/app/src-tauri/src/coordinator/resources.rs` (lines 1-160)
- `openless-all/app/src-tauri/src/coordinator_state.rs` (full file, 1-485)
- `openless-all/app/src-tauri/src/global_hotkey_runtime.rs` (full file, 1-107)
- `openless-all/app/src-tauri/src/audio_mute.rs` (lines 1-263)
- `openless-all/app/src-tauri/src/permissions.rs` (lines 200-360)
- `openless-all/app/src-tauri/src/insertion.rs` (lines 1-150, 300-450)
- `openless-all/app/src-tauri/src/persistence.rs` (lines 146-156, 785-811)
- `openless-all/app/src-tauri/src/recorder.rs` (lines 28-490)
- `openless-all/app/src-tauri/src/commands.rs` (lines 975-1000)
- `openless-all/app/src/lib/types.ts` (lines 118-275)
- `openless-all/app/src/lib/ipc.ts` (lines 40-176)
- `openless-all/app/src/components/Capsule.tsx` (line 293)
- `openless-all/app/src/pages/QaPanel.tsx` (lines 55, 116-120)
- `openless-all/app/src/pages/Vocab.tsx` (line 51)
