# PR #940 审查发现清单（待修复）

> 本文档由代码审查产出，供接手人核对并修复 PR #940 中发现的问题。
> 所有行号基于 PR 当前头部提交 `5bcfa55`（分支 `feat/local-whisper-provider`）与 `beta` 的差集。

## 1. 元信息

| 项 | 值 |
| --- | --- |
| PR | https://github.com/Open-Less/openless/pull/940 |
| 标题 | 本地模型增加 whisper 相关模型的支持；增加 qwen3-asr 的 rust 版本，相比 C 版本速度更快 |
| 目标分支 | `Open-Less:beta` |
| 审查基准 | `beta` → `5bcfa55`（54 文件，+2813 / −364） |
| 审查方式 | 逐文件代码阅读 + 独立 review 子代理 + 前端测试实跑 + `cargo` 复现验证 |
| 已验证 | 前端测试套件全绿（EXIT=0，含新增 `ChannelList.test.ts` 平台过滤 / 旧渠道兼容断言） |
| 未验证 | macOS（aarch64 + Intel）与 Linux 的 Rust 编译；PR 当前无任何 CI checks 报告 |

## 2. 结论摘要

- **方向与代码质量良好**：cfg gating、缓存键控、生命周期串行化、旧渠道兼容均实现正确，没有发现必须改代码的硬伤。
- **合并前必须完成**：① 跑通 macOS aarch64（MLX）与 Linux（C 后端）两条全新编译面；② 修复 2 个 should-fix；③ 明确开发者本地 `cargo check` 需要子模块的回归。
- **重要纠正**：前序审查认为 `ci-disable-macos-qwen3.mjs` 在 target gating 下"多余、可删除"——**该结论错误**，见 §3.0。

## 3. 问题清单

### 3.0 对前序审查的纠正（先看这个，防止误删承重代码）

**位置**：`.github/workflows/{android-apk,ci,release-tauri}.yml` 中的 `ci-disable-macos-qwen3.mjs` 调用；脚本本体 `openless-all/app/scripts/ci-disable-macos-qwen3.mjs`。

前序审查声称：`qwen3-asr-rs` 已在 `Cargo.toml:132` 用 `cfg(all(target_os = "macos", target_arch = "aarch64"))` 门控，非 macOS 构建不会解析它，因此 CI 里"删依赖行"的脚本多余。

**实测结论（Windows 宿主，未初始化子模块）**：

```
$ cargo check --manifest-path openless-all/app/src-tauri/Cargo.toml
error: failed to get `qwen3-asr-rs` as a dependency of package `openless`
Caused by: failed to load source for dependency `qwen3-asr-rs`
Caused by: unable to update ...\src-tauri\vendor\qwen3-asr-rs
Caused by: failed to read `...\vendor\qwen3-asr-rs\Cargo.toml`
```

Cargo 解析器对 **path 依赖会跨所有 target 解析 manifest**，与 cfg gating 无关。因此：

- 非 macOS CI 用 `submodules: false` 时，**没有该脚本 `cargo check --locked` 必然失败**——脚本是承重的，不能删除。
- 该脚本的副作用仍然成立：CI 校验的是被改过的 manifest、正则脆弱（依赖行一旦换行/格式化即硬失败 CI）、`Cargo.lock` 残留 `qwen3-asr-rs` 条目。
- 若想消除副作用，替代方案（任选）：始终在 CI 初始化子模块；或把 `qwen3-asr-rs` 改为可选 feature（`cargo check --no-default-features`）；或将 path 依赖声明移到 `[target.'cfg(...)'.dependencies]` 之外再用 `[patch]` 兜底。**修复时不要简单删脚本。**

### P1-1 macOS aarch64（MLX）构建链路完全未验证

**位置**：`Cargo.toml:132`（`qwen3-asr-rs = { path = "vendor/qwen3-asr-rs", default-features = false, features = ["mlx"] }`）、`openless-all/app/src-tauri/vendor/qwen3-asr-rs`（子模块 pin `7a73063`）、嵌套子模块 `jimersylee/mlx-c`、`Cargo.lock`（`bindgen 0.71.1`、`cmake`、`tokenizers 0.21.4`、`whisper-rs 0.14.4`）。

整条链都是**新编译面**，且 PR 当前无 CI checks：

1. `qwen3_asr_rs` 上游 `[features] mlx = []` 是**空 feature**——MLX 后端不经过 cargo 依赖，而是由其 `build.rs` 用 `cmake` + `bindgen` 编译 vendored `mlx-c` 子模块并链接 Metal。这要求构建机有 `cmake`、`libclang`、MetalToolchain，三者缺一即失败。
2. 嵌套子模块 `.gitmodules`（`jimersylee/qwen3_asr_rs@7a73063` 内）已将 `mlx-c` 指向 `https://github.com/jimersylee/mlx-c.git`（HTTPS，前序 P1 的 SSH/URL 问题已修复），但**该仓库中 pin 的 gitlink commit 是否存在未能核实**（GitHub API 限流），递归 `git submodule update --init --recursive` 是否真能成功仍是未知数。
3. `whisper-rs 0.14.4`（metal feature）同样只在本机无法编译验证。

**影响**：若任意一环失败，macOS 发布构建直接红色，且问题发生在编译早期（子模块 fetch / cmake / bindgen）。

**验收标准**：`beta` 合并前，在 `macos-14`（或更高）runner 上跑通 `cargo check` + `cargo build --release`（aarch64），并确认 `git submodule update --init --recursive` 无报错。

### P1-2 Linux C 后端编译面未验证

**位置**：`build.rs` 的 `build_qwen_asr("linux")`（`build.rs:64` 起）。

`qwen_asr.c` 首次在 Linux 上编译：无 `USE_BLAS` / `ACCELERATE_NEW_LAPACK`，追加 `-O3 -ffast-math`，链接 `-lm -lpthread`。该 C 代码此前只在 macOS 编译过，Linux 路径（通用 CPU kernels）是全新编译面。

**验收标准**：Linux CI job 跑通 `cargo check` + `cargo build`（`ci.yml` 已加 "Initialize Linux C ASR submodule" 步骤初始化 `vendor/qwen-asr`，需确认该子模块路径与 `--manifest-path src-tauri/Cargo.toml` 的相对位置一致）。

### P1-3 开发者本地 `cargo check` 回归（Windows / Linux）

**现象**：beta 上 Windows / Linux 开发者 `cargo check` 不需要任何子模块；本 PR 后，未执行 `git submodule update --init --recursive` 直接 `cargo check` 会硬失败（见 §3.0 的复现）。`Cargo.toml:132` 的 path 依赖使解析器必须能读到 `vendor/qwen3-asr-rs/Cargo.toml`。

**现状缓解**：README（`README.md` / `README.zh.md`）已新增 "Apple Silicon 编译需要 MetalToolchain" 说明，且原有 `git submodule update --init --recursive` 指引仍在。但这比 beta 更脆弱——**Linux 开发者**现在还需要 `vendor/qwen-asr` 子模块（build.rs 编译用），Windows 开发者则需要 `qwen3-asr-rs` 子模块（解析用）。

**建议**：文档中显式说明"所有平台源码构建都需要递归初始化子模块"，或在 `Cargo.toml` 依赖行上方加注释说明该依赖被解析（而非仅编译）于所有平台。

### SF-1 `strip` 全局改动应收窄到 macOS

**位置**：`Cargo.toml:181` `strip = "debuginfo"`（原 `strip = "symbols"`）。

为修复 macOS Homebrew rustc 的 proc-macro dylib 问题（`mis-aligned LINKEDIT string pool`），把 release profile 的 strip 从 `symbols` 降级为 `debuginfo`，但这是**全局**改动：Windows / Linux / Android 的发布产物现在保留完整符号表（体积变大、符号名暴露）。

**建议**：改为 target 限定：

```toml
[target.'cfg(target_os = "macos")'.profile.release]
strip = "debuginfo"
```

并在全局 profile 保留 `strip = "symbols"`。若该问题确实只发生在 macOS Homebrew rustc，此改动无副作用。

### SF-2 超时后 `cancel()` 是无效调用（liveness 问题）

**位置**（三处一致模式）：

- `src/coordinator/dictation.rs:3706-3709`
- `src/coordinator.rs:2559-2562`
- `src/coordinator/qa_session.rs:598-601` 与 `1428-1431`

```rust
let local_for_cancel = Arc::clone(&local);
let result = tokio::time::timeout(timeout_duration, local.transcribe()).await;
if result.is_err() {
    local_for_cancel.cancel();   // ← 无效
}
```

**问题**：`LocalQwenAsr::transcribe()` 开头已 `mem::take` 缓冲并 `spawn_blocking` 派发解码。超时后 `cancel()` 只把 `AtomicBool` 置位（仅关闭后续 token 事件门控），**无法中止正在运行的解码任务**：orphaned `spawn_blocking` 仍持着引擎的 context 锁（C 后端 `Mutex<WhisperContext>` / MLX `Mutex<AsrInference>`）跑完，下一次会话的 `transcribe` 会阻塞等它结束；`LocalWhisperAsr` 则完全没有取消路径。超时是低频路径，但一旦触发会造成"下次会话卡住"的观感。

**建议**（任选）：

- 超时后对 cache 调用 `release_now()` 驱逐引擎——下次会话加载新引擎，而不是排队等旧任务（旧任务持有的 `Arc` 会在完成后自动释放内存）；
- 或为解码任务加共享 cancel 标志（在 chunk 边界检查）实现真正的中止；
- 至少：删掉误导性的 `cancel()` 调用并注释说明"超时只放弃结果，引擎由锁串行化"。

**验收标准**：人为把 timeout 调到极小，连续两次会话不出现明显阻塞；或补充一个描述该语义的单元/注释。

### N-1 Turbo ↔ Q5 同目录导致 UI 状态与后端就绪不一致

**位置**：`src/asr/local/models.rs:114-129`（`model_dir` 目录共享）、`models.rs:131-140`（`is_downloaded` 只看目标文件）、`models.rs:213-222`（`delete_model` 只删目标文件）、`src/asr/local/whisper_provider.rs`（`model_path_for_model` 对 Turbo 有 q5 回退）。

**问题**：`WhisperLargeV3Turbo` 与 `WhisperLargeV3TurboQ5` 共用 `whisper-large-v3-turbo/` 目录。用户只下载 q5 文件时：

- 后端 `model_ready_for_model("whisper-large-v3-turbo")` 通过 q5 回退返回 `true`（可用）；
- 但 UI 的 `is_downloaded` / `downloaded_bytes` 仍按 `ggml-large-v3-turbo.bin` 判断，显示"未下载 / 0 字节"；
- `delete_model(Turbo)` 只删 turbo 文件不删 q5，反之亦然。

**建议**：让目录共享的两个 id 在就绪判断/字节数/删除上互相认账（例如就绪 = 任一目标文件存在；删除 = 两个文件都删），或在 UI 侧明确"q5 是 turbo 的替代格式"。

### N-2 MLX 临时 WAV 文件无 RAII 清理

**位置**：`src/asr/local/mlx_qwen_engine.rs:36-51`（`transcribe_pcm`）。

临时 WAV 在 `transcribe` 返回后才 `remove_file`；解码 panic / 进程被杀会泄漏文件到系统临时目录。建议用 RAII guard（`tempfile` 已在 `Cargo.lock` 中可用）。

### N-3 HF 文件列表接口无分页

**位置**：`src/asr/local/download.rs:114-145`（`fetch_file_list`）。

`/api/models/{repo}/tree/main` 单页假设目标文件在 repo 根且条目 < 1000。当前 `ggerganov/whisper.cpp` 成立；但单页请求失败时 `files.is_empty()` 会让所有 whisper 模型下载整体失败。可改为直接 `resolve/main/{file}` 单文件探测，或加分页。

### N-4 Intel Mac 添加渠道下拉短暂闪现 MLX 预设

**位置**：`src/pages/settings/ChannelList.tsx:158,173`。

`supportsQwen3Mlx` 初值 `os === 'mac'`，随后由 `getPlatformCapabilities()` 异步纠正——Intel Mac 用户打开添加渠道下拉时，MLX 选项会闪现一下再消失。测试只覆盖稳态。可改为初值 `false` + 异步置 true（Apple Silicon 上 MLX 会晚一帧出现，可接受），或初值直接走同步的 `inferPlatformCapabilities()`。

### N-5 `MetalToolchainGuide` 对所有 Apple Silicon 无条件显示

**位置**：`src/pages/LocalAsr/index.tsx:2038`（`{supportsQwen3Mlx && <MetalToolchainGuide />}`）、`src/pages/LocalAsr/components.tsx`（组件本体）。

Toolchain 缺失时 `pretauri` 会直接阻止应用启动，真正缺失的用户看不到应用内引导（只能看到终端错误），而**已装好** Toolchain 的用户反而看到无意义的引导块。建议：改为运行时探测（如由前端调用 `verify` 命令按缺失状态显示），或至少把文案改为"源码构建期要求"并默认折叠（当前 `defaultOpen={import.meta.env.DEV}`）。

### N-6 供应链：`qwen3-asr-rs` pin 在个人 fork —— ✅ 已解决

**位置**：`.gitmodules`（`jimersylee/qwen3_asr_rs.git`）、`Cargo.lock` 中 `qwen3-asr-rs 0.2.0`。

上游是 `second-state/qwen3_asr_rs`，本 PR 原 pin 的是 `jimersylee` 个人 fork（含 `mlx-c` 子模块与补丁）。与 `qwen-asr` 子模块放在 `Open-Less` org 的做法不一致：个人仓库被删/改名即断构建。

**修复（2026-08-17）**：fork 已镜像到 `Open-Less/qwen3_asr_rs`（`openless-patches` 分支 = pin `7a73063`，已验证可从新 URL 正常拉取），`.gitmodules` 顶层 URL 已改为 `https://github.com/Open-Less/qwen3_asr_rs.git`。

**剩余风险**：嵌套子模块 `mlx-c` 仍指向 `https://github.com/jimersylee/mlx-c.git`（`Open-Less` org 下暂无镜像）。合并前建议同步镜像 `mlx-c` 到 org 并更新嵌套 `.gitmodules`（需在镜像仓库内提交）。

### N-7 陈旧注释

**位置**：`.github/workflows/ci.yml`（注释仍引用 `build.rs:49 build_qwen_asr_macos`，实际逻辑已改为 TARGET 解析）；`docs/qwen-asr-submodule-upgrade-checklist.md` 已正确更新，可参照。

## 4. 已确认正确的部分（无需重复排查）

- **cfg gating**：`qwen3-asr-rs` 仅 `macos + aarch64`（`Cargo.toml:130-132`）；whisper 仅 macOS；C 后端 macOS + Linux；`build.rs` 用 `TARGET` 解析排除 Android（`build.rs:12-24`）。
- **后端缓存**：`LocalAsrCache` 按 `(backend, model_id)` 键控（`src/asr/local/cache.rs`），MLX ↔ C 不互蹭；`QwenBackend::cache_key` 区分 `mlx` / `c`。
- **加载生命周期**：`local_asr_lifecycle` 门闩串行化加载与释放；`load_current_local_qwen_engine` 在加载前后都校验目标 provider 未切换，切换则丢弃（`src/coordinator/asr_wiring.rs`）。
- **切换释放顺序**：`set_active_asr_provider` 先释放所有非目标 runtime 再预加载（`src/commands/credentials.rs`），避免两套大模型同时驻留。
- **旧渠道兼容**：`presetsFor` 编辑时把当前 provider id 补回选项、拒绝未知 id 注入（`src/pages/settings/ChannelList.tsx:56-75`）；`local-qwen3` 按 OS 映射后端（`src/asr/local/mod.rs` `qwen_backend_for_provider`）；均有 `ChannelList.test.ts` 覆盖并通过。
- **C 后端语义保留**：dictation 路径仍走 stream + `local-asr-token` + 0.5s 尾部静音收尾（`src/asr/local/mod.rs` `transcribe_dictation_with_handler`、`local_provider.rs`），并在子模块升级清单中声明；MLX 为 batch、无流式 token（胶囊需等最终文本——已确认是设计行为）。
- **子模块 URL**：顶层与嵌套均改为 HTTPS；前序 P1 的 SSH URL / gitlink 指向问题已修复。
- **前端**：Capsule 的 `local-asr-token` 订阅/清理、录音时重置文本均正确（`src/components/Capsule.tsx`）。

## 5. 复现与验证命令

```bash
# 前端测试（已在 beta..5bcfa55 上实跑通过，EXIT=0）
node openless-all/app/scripts/frontend-test-runner.mjs

# 复现 P1-3 / §3.0：未初始化子模块时 cargo 解析失败（Windows/Linux/macOS 通用）
cargo check --manifest-path openless-all/app/src-tauri/Cargo.toml
# 预期：failed to read vendor/qwen3-asr-rs/Cargo.toml

# 递归子模块初始化（合并前必须在干净环境验证，含嵌套 mlx-c）
git submodule update --init --recursive

# 类型检查 / 构建（tsc + vite）
cd openless-all/app && pnpm build
```

## 6. 合并前验证清单

- [ ] macOS aarch64：`git submodule update --init --recursive` + `cargo check` + `cargo build --release`（含 `qwen3-asr-rs` MLX 与 `whisper-rs` metal）——P1-1
- [ ] macOS Intel：确认不编译 MLX、走 C 后端、`verify-macos-metal-toolchain.mjs` 提前退出——P1-1
- [ ] Linux：`cargo check` + `cargo build`（C 后端编译面）——P1-2
- [ ] Windows：`cargo check --locked`（依赖 `ci-disable-macos-qwen3.mjs`，确认脚本正则仍匹配）——§3.0
- [ ] Android：`cargo check --target aarch64-linux-android`（确认 `build.rs` 的 TARGET 解析排除 qwen-asr）——P1-2
- [ ] `strip = "debuginfo"` 收窄到 macOS（SF-1）
- [ ] 超时 cancel 语义处理（SF-2）
- [x] `qwen3-asr-rs` fork 已镜像到 `Open-Less` org、`.gitmodules` 已指向 org 仓库（N-6；嵌套 `mlx-c` 尚未镜像，见 N-6 剩余风险）
