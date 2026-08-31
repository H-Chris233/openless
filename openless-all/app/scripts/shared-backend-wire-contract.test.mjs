import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

const read = (path) => readFile(new URL(`../${path}`, import.meta.url), 'utf8');
const [
  types,
  qaPanel,
  remoteIpc,
  remoteSection,
  selectionVoiceIpc,
  remoteCommand,
  tauriEvents,
  lessComputerIpc,
  lessComputerPanel,
  qaCommand,
  remoteServer,
  coordinator,
  coordinatorAsrWiring,
  coordinatorDictation,
  coordinatorCapsule,
  coordinatorHotkeys,
  tauriCoordinatorHost,
  coreAdapters,
  providersCommand,
  qaAdapter,
  selectionVoiceCoordinator,
  dictionaryCommand,
  stylePacksCommand,
] =
  await Promise.all([
    read('src/lib/types.ts'),
    read('src/pages/QaPanel.tsx'),
    read('src/lib/ipc/remote-server.ts'),
    read('src/pages/settings/RemoteInputSection.tsx'),
    read('src/lib/ipc/selection-voice-preview.ts'),
    read('src-tauri/src/commands/remote_input.rs'),
    read('src-tauri/src/tauri_events.rs'),
    read('src/lib/ipc/less-computer.ts'),
    read('src/pages/LessComputerPanel.tsx'),
    read('src-tauri/src/commands/qa.rs'),
    read('src-tauri/src/remote_server/mod.rs'),
    read('src-tauri/src/coordinator.rs'),
    read('src-tauri/src/coordinator/asr_wiring.rs'),
    read('src-tauri/src/coordinator/dictation.rs'),
    read('src-tauri/src/coordinator/capsule_focus.rs'),
    read('src-tauri/src/coordinator/hotkey_loops.rs'),
    read('src-tauri/src/tauri_coordinator_host.rs'),
    read('src-tauri/src/core_adapters.rs'),
    read('src-tauri/src/commands/providers.rs'),
    read('src-tauri/src/qa_adapter.rs'),
    read('src-tauri/src/coordinator/selection_voice_session.rs'),
    read('src-tauri/src/commands/dictionary.rs'),
    read('src-tauri/src/commands/style_packs.rs'),
  ]);

for (const kind of ['awaiting_approval', 'cancelled', 'error']) {
  assert.match(types, new RegExp(`\\| '${kind}'`), `QaStateKind must retain ${kind}`);
  assert.match(qaPanel, new RegExp(`case '${kind}':`), `QaPanel must handle ${kind}`);
}
for (const field of [
  'session_id',
  'messages',
  'selection_preview',
  'chunk',
  'error',
  'edit_instruction_mode',
  'edit_apply_available',
  'edit_revert_available',
  'approval_token',
]) {
  assert.match(types, new RegExp(`\\b${field}\\?`), `QaStatePayload is missing ${field}`);
}
assert.match(qaPanel, /listen<QaStatePayload>\('qa:state'/, 'QA must consume the typed state event');

const remoteStatus = remoteIpc.match(/export interface RemoteInputStatus \{([\s\S]*?)\n\}/)?.[1];
assert.ok(remoteStatus, 'RemoteInputStatus interface must exist');
const remoteFields = [...remoteStatus.matchAll(/^\s+(\w+):/gm)].map((match) => match[1]);
assert.deepEqual(
  remoteFields,
  ['running', 'starting', 'port', 'pin', 'urls', 'urlsStale'],
  'the explicit remote status command must preserve the beta secret wire shape',
);
assert.match(remoteIpc, /invokeOrMock\("get_remote_input_status"/, 'remote status command name drifted');
assert.match(remoteSection, /listen\('remote-input:running'/, 'remote running event must refresh status');
assert.match(remoteSection, /listen\('remote-input:error'/, 'remote errors must remain visible');
assert.match(
  remoteCommand,
  /read_pairing_pin\(\)[\s\S]*?map_remote_input_status\(status, pin\)/,
  'PIN must only be added by the explicit status command conversion',
);
assert.match(
  tauriEvents,
  /QaStateEvent::from_snapshot\(&snapshot\)/,
  'lagged resync must rebuild QA from the Core snapshot',
);
assert.match(
  tauriEvents,
  /RemoteInputRuntimeEvent::from\(&status\)/,
  'lagged resync must rebuild Remote Input without a PIN',
);

for (const [command, args] of [
  ['get_selection_voice_preview', '{ qaSessionId }'],
  ['confirm_selection_voice_preview', '{ text, qaSessionId }'],
  ['revert_selection_voice_preview', '{ qaSessionId }'],
]) {
  assert(
    selectionVoiceIpc.includes(`'${command}', ${args}`),
    `${command} wire drifted`,
  );
}

for (const field of ['events', 'oldestSequence', 'latestSequence', 'truncated']) {
  assert.match(types, new RegExp(`\\b${field}[?:]`), `LessComputerSyncResult is missing ${field}`);
}
assert.match(
  lessComputerIpc,
  /less_computer_sync[\s\S]*?\{ afterSequence \}/,
  'Less Computer sync must send its applied sequence waterline',
);
assert.match(
  qaCommand,
  /less_computer_sync[\s\S]*?after_sequence: Option<u64>[\s\S]*?LessComputerEventReplay/,
  'the Tauri sync command must return bounded replay metadata',
);
assert.match(
  lessComputerPanel,
  /reconcileLessComputerReplay\(lcAppliedSeq, replay, pending\)/,
  'Less Computer must merge replay with events buffered during listener installation',
);
assert.match(
  lessComputerPanel,
  /reconciled\.reset\) setTurns\(\[\]\)/,
  'a truncated replay must reset the derived Less Computer view',
);

assert.match(
  remoteServer,
  /match authed \{[^]*?AuthResult::BadPin[^]*?return;[^]*?AuthResult::Locked[^]*?return;[^]*?\.connect\(connection_id\)/,
  'Remote Input must reject an invalid PIN before creating a Core connection',
);
assert.match(
  remoteServer,
  /verify_hello[^]*?constant_time_eq\(p\.as_bytes\(\), state\.pin\.as_bytes\(\)\)/,
  'Remote Input PIN verification must retain constant-time comparison',
);
assert.match(
  remoteServer,
  /apply_remote_control\([^]*?state\.backend\.services\(\)\.remote_input\.as_ref\(\)/,
  'Remote Input WebSocket control must use the Core lifecycle',
);
assert.match(
  remoteServer,
  /\.remote_input[^]*?\.disconnect\(connection_id\)/,
  'Remote Input WebSocket disconnect must release the Core connection',
);

const coordinatorBusinessSources = [
  coordinator,
  coordinatorAsrWiring,
  coordinatorDictation,
  coordinatorCapsule,
  coordinatorHotkeys,
].join('\n');
for (const eventName of ['local-asr-token', 'remote:result']) {
  assert.doesNotMatch(
    coordinatorBusinessSources,
    new RegExp(`\\.emit\\(\\s*["']${eventName}["']`),
    `${eventName} must be derived from typed Core events by the centralized Tauri bridge`,
  );
}
assert.match(
  coordinatorBusinessSources,
  /BackendEventKind::TranscriptDelta/,
  'legacy Sherpa progress must publish a typed Core transcript delta',
);
assert.match(
  coordinatorBusinessSources,
  /BackendEventKind::DictationCompleted/,
  'legacy completion must publish a typed Core result',
);
const coordinatorInner = coordinator.match(/struct Inner \{([^]*?)\n\}/)?.[1];
assert.ok(coordinatorInner, 'Coordinator Inner must remain inspectable by the architecture contract');
assert.doesNotMatch(
  coordinatorInner,
  /AppHandle|AppHandleSlot|\bapp\s*:/,
  'Coordinator must not retain a Tauri AppHandle',
);
assert.match(
  coordinatorInner,
  /host: crate::tauri_coordinator_host::TauriCoordinatorHost/,
  'Coordinator must depend on the explicit Tauri host Module',
);
assert.doesNotMatch(
  coordinatorBusinessSources,
  /\.emit(?:_to)?\(/,
  'Coordinator modules must publish typed Core events or call semantic Tauri host actions',
);
assert.match(
  tauriCoordinatorHost,
  /struct TauriCoordinatorHost[^{]*\{[^]*?AppHandleSlot/,
  'the late-bound AppHandle must be owned by the Tauri host Module',
);
assert.doesNotMatch(
  tauriCoordinatorHost,
  /crate::coordinator::(?:Inner|capsule_focus)/,
  'the Tauri host must not reach back into Coordinator internals to operate capsule windows',
);
assert.doesNotMatch(
  coordinator,
  /use tauri::AppHandle|fn bind_app\s*\(/,
  'Coordinator must expose its Tauri host seam instead of accepting AppHandle directly',
);
assert.match(
  coordinator,
  /\nmod capsule_focus;/,
  'capsule_focus must remain private to the Coordinator module',
);
assert.doesNotMatch(
  coreAdapters,
  /managed_coordinator|try_state::<Arc<crate::coordinator::Coordinator>>/,
  'Core adapters must receive narrow shared host state instead of reaching back into Coordinator',
);
for (const legacyProviderCopy of [
  /#\[cfg\(any\(\)\)\]/,
  /\bTauriCloudTranscriptionEngine\b/,
  /\bTauriCloudTextPolisher\b/,
  /\bTauriOmniDictationEngine\b/,
  /\bbuild_tauri_omni_provider\b/,
]) {
  assert.doesNotMatch(
    coreAdapters,
    legacyProviderCopy,
    'Tauri must not retain a disabled copy of provider protocol construction owned by openless-core',
  );
}
assert.match(
  providersCommand,
  /\.provider\s*\n?\s*\.validate\(/,
  'Tauri provider validation command must delegate to Core ProviderApi',
);
assert.match(
  providersCommand,
  /\.provider\s*\n?\s*\.list_models\(/,
  'Tauri provider model-list command must delegate to Core ProviderApi',
);
for (const forbiddenProviderBusinessToken of [
  /CredentialsVault/,
  /ProviderScope/,
  /ProviderConfig/,
  /reqwest::/,
  /validate_provider_service/,
  /list_provider_models_service/,
  /tokio::time::timeout/,
  /build_active_omni_provider/,
]) {
  assert.doesNotMatch(
    providersCommand,
    forbiddenProviderBusinessToken,
    'Tauri provider commands must not recreate Core credential/protocol business logic',
  );
}
assert.match(
  coordinatorInner,
  /hotkey_status: Arc<Mutex<HotkeyStatus>>/,
  'Coordinator and the platform adapter must share one hotkey status slot',
);
assert.match(
  coordinatorInner,
  /qa_context: Arc<TauriQaHostContext>/,
  'Coordinator and QA adapters must share one QA host context',
);
assert.match(
  qaAdapter,
  /fn is_panel_visible\(&self\)[^]*?panel_visible\.load\(Ordering::Acquire\)/,
  'QA host visibility must be read from the shared atomic context',
);
assert.match(
  selectionVoiceCoordinator,
  /\.process_transcript\(session_id, transcript\)/,
  'the Tauri selection-voice adapter must submit raw ASR text to the Core workflow',
);
assert.match(
  selectionVoiceCoordinator,
  /\.prepare_edit\(session_id, None\)/,
  'the Tauri selection-voice adapter must consume the Core-owned edit delivery decision',
);
for (const forbiddenBusinessToken of [
  /apply_correction_rules/,
  /list_correction_rules/,
  /polish_text/,
  /translate_text/,
  /parse_edit_plan/,
  /apply_edit_plan/,
  /voice_edit_system_prompt/,
  /selection_voice_intent_classification_prompt/,
  /infer_selection_voice_translation_target/,
  /selection_polish_output_mode/,
]) {
  assert.doesNotMatch(
    selectionVoiceCoordinator,
    forbiddenBusinessToken,
    'Selection Voice correction, prompting, intent, EditPlan and delivery policy must remain in openless-core',
  );
}
assert.match(
  qaAdapter,
  /\.edit_preview\(SelectionVoiceEditRequest/,
  'the QA adapter must delegate preview generation and revision to openless-core',
);
assert.doesNotMatch(
  qaAdapter,
  /parse_edit_plan|apply_edit_plan|generate_edit_plan|voice_edit_system_prompt/,
  'the QA adapter must not recreate the Core selection-edit workflow',
);
assert.doesNotMatch(
  qaAdapter,
  /try_state::<Arc<crate::coordinator::Coordinator>>|Tauri coordinator state is unavailable/,
  'the QA adapter must use its narrow host callback instead of looking up Coordinator state',
);
assert.match(
  qaAdapter,
  /set_selection_voice_target_binder|bind_selection_voice_target/,
  'the QA adapter must expose the narrow opaque-target host seam',
);

for (const method of [
  'accept_pending_correction',
  'reject_pending_correction',
  'dismiss_pending_corrections',
]) {
  assert.match(
    dictionaryCommand,
    new RegExp(`core\\.${method}\\(`),
    `vocabulary suggestion command must delegate ${method} to Core`,
  );
}
assert.match(
  dictionaryCommand,
  /refresh_vocab_suggestion_presentation/,
  'the Tauri command may only pass Core suggestion presence to the host presentation seam',
);
assert.doesNotMatch(
  dictionaryCommand,
  /coord\.(?:accept_pending_correction|reject_pending_correction|dismiss_vocab_suggestions)\(/,
  'Tauri Coordinator must not own vocabulary suggestion mutations',
);
assert.match(
  qaCommand,
  /less_computer_window_dismiss\([^]*?core\.services\(\)\.less_computer\.dismiss\(\)/,
  'Less Computer dismiss must clear the Core conversation before hiding the host window',
);
assert.match(
  qaCommand,
  /less_computer_submit_text\([^]*?(?:core|backend)\.submit_less_computer\(text\)/,
  'Less Computer text submit must delegate the run to Core',
);
assert.doesNotMatch(
  coordinator,
  /pub fn less_computer_(?:window_dismiss|window_open|submit_text)\(/,
  'Coordinator must not own Less Computer command business or window wrappers',
);
assert.match(
  stylePacksCommand,
  /core\.preview_style_pack_runtime\(&style_pack\)/,
  'style-pack runtime diagnostics must be assembled by Core',
);
const stylePackPreviewCommand = stylePacksCommand.match(
  /pub fn preview_style_pack_runtime\([^]*?\r?\n\}\r?\n/,
)?.[0];
assert.ok(stylePackPreviewCommand, 'style-pack preview command must remain present');
assert.doesNotMatch(
  stylePackPreviewCommand,
  /CoordinatorState/,
  'style-pack commands must not reach back into Coordinator for business diagnostics',
);
assert.match(
  coordinatorBusinessSources,
  /backend\.asr_vocabulary_phrases\(\)/,
  'ASR hotword ordering must come from the shared Core vocabulary projection',
);
assert.doesNotMatch(
  coordinatorBusinessSources,
  /(?:fn|const)\s+(?:asr_vocab_phrases|prioritize_vocab_for_asr|FRESH_VOCAB_SEATS)\b/,
  'Coordinator must not own the Core ASR vocabulary priority rule',
);
for (const legacyDictationFacade of [
  'start_dictation',
  'start_dictation_with_translation',
  'stop_dictation',
  'stop_dictation_with_translation',
  'cancel_dictation',
]) {
  assert.doesNotMatch(
    coordinator,
    new RegExp(`\\bpub(?:\\(crate\\))?\\s+(?:async\\s+)?fn\\s+${legacyDictationFacade}\\b`),
    `Coordinator must not expose the legacy ${legacyDictationFacade} facade; production entry points use openless-core`,
  );
}

console.log('shared-backend-wire-contract.test.mjs passed');
