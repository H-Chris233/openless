# Core / Host 平台边界审计

审计基线：OpenLess 2.0.0-Beta.1（2026-09-01）。范围覆盖 `openless-core`、Tauri、Linux egui、React/TypeScript、C++ 插件及构建脚本；vendor、生成物和纯视觉实现排除。

## 已迁移项

- Provider 解析、ASR/LLM/Omni、设置事务、事件、Less Computer policy 和四种 Agent DTO 位于 Core。
- `LessComputerVoiceSession` 在 Core 持有 session lease、ASR snapshot、PCM 校验、TranscriptDelta 和 Agent submit。
- Hold/Toggle/Auto/Combined 热键解释由 Core `DictationHotkeyEdge` 复用；Auto 阈值为 350ms。

## 本批修复项

- Shared realtime ASR 将 Qwen、StepFun、Bailian、Volcengine、讯飞 interim 回调接入统一 `TextStreamSink`。
- Linux host 增加 Less Computer pressed/released/combined typed events，并把 PCM/finish/cancel 路由到 Core session。
- Linux/Tauri manifest、打包脚本和 AppStream 使用 `AGPL-3.0-only`；2.0.0-Beta.1 为许可证生效边界，1.x 发布物仍为 MIT。

## 有意保留的 Host 项

- Tauri 窗口、胶囊、原生录音/native ASR、系统凭据、插入和生命周期。
- Linux cpal、fcitx5、Secret Service、资源布局和单实例。
- Windows Foundry/Sherpa、macOS Apple Speech/MLX/Whisper 等单平台 runtime。

## Deferred 候选

- 将 Tauri 进程 runner 的全部 provider transport 进一步拆成独立 Core crate。
- 将 Tauri Coordinator 的旧 voice recorder/ASR 兼容编排完全替换为 `LessComputerVoiceSession`。
- Linux Generic/Qwen 本地模型的真实下载、缓存和设备 runner 证明。
- Ubuntu 实际焦点输入、音频设备、签名安装以及 Android/macOS/Windows 设备 smoke。

## Standards

依赖方向保持 Host → Core；Core 不引用 Tauri/egui，凭据只经 `CredentialStore`，未注入 runtime 明确返回 `Unsupported`。共享 session 通过一个 mutable lease 和 `BackendEvent.session_id` 关联。

## Spec

2.0 目标的 Core voice session、实时增量、三档热键解释和 Linux typed event 已有公共入口；平台原生录音/本地模型仍由 Adapter 提供，不能以 headless fixture 代替设备证据。

## 剩余风险和证据边界

本审计证明源码边界和可运行 contract，不证明云服务凭据、真实音频设备、CLI 安装、签名、设备运行或发布产物。Backend contract 版本继续为 `1.0.0`，不随应用版本升级。
