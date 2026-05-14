# #420 Wayland 支持方案说明

> 适用范围：`/home/chris233/openless`
> 关联 issue：[#420](https://github.com/Open-Less/openless/issues/420)
> 目标：给 OpenLess 在 Linux / Wayland 下补一条可靠、与当前仓库决策一致的实现路径，而不是继续把 X11 思路硬套过去。

## 1. 当前问题拆分

#420 现在实际上混了三类问题：

1. **Wayland 下全局快捷键不可用**
   - 这是因为 Wayland 安全模型不允许普通应用像 X11 那样监听全局键盘事件。
   - 当前仓库已经把 CLI + single-instance 路径做成 Wayland 下的正式可交付方案；portal 仍属于后续研究方向，而不是现阶段已落定的主实现。

2. **Wayland 下文本输出不可靠**
   - 流式输出路径：`unicode_keystroke.rs` 在 Linux 仍走 `enigo.text(...)`。
   - 一次性输出路径：`insertion.rs` 仍走 `clipboard + simulate_paste(enigo)`。
   - 这两条路径本质都还是 X11 风格假设，在 Wayland 下可能“调用成功但没真正落字”。

3. **Wayland 下设置页快捷键录制 / UI 黑屏闪烁**
   - 这更像 WebKitGTK / 合成器 / 输入录制 UI 的独立问题。
   - 不应继续和“Wayland 全局快捷键”或“Wayland 文本输出”混成一个修复面。

## 2. 关键判断

### 2.1 Wayland 有多层可行路径，但不能把尚未验证的 portal 能力写成既定主路线

必须分开看：

- **全局快捷键触发**：
  - 从协议方向看，portal / compositor 能力值得研究；
  - 但从**当前仓库已落地实现**与跨桌面可交付性看，正式支持路径已经是 `CLI + single-instance 转发`；
  - `xdg-desktop-portal` 的 `GlobalShortcuts` 现阶段更适合作为 research track，而不是直接写成产品承诺。
- **文本插入**：没有 X11 那种“应用可随意向其他应用发键”的通用能力。
  - 剪贴板有现实可行路。
  - 自动输入只能走 **受权限控制** 的 portal / libei / compositor 能力。
  - 不存在一个对所有 Wayland 桌面都等价、无感、无授权的统一注入接口。

### 2.2 现阶段最高优先级不是“自动输入一步到位”，而是“用户文本不能丢”

当前最危险的问题不是“Wayland 下体验不够自动化”，而是：

- 日志显示成功
- OpenLess 认为已经插入
- 用户实际输入框里没有字

这个行为会直接破坏产品的核心承诺：**用户的话不能丢**。

## 3. 建议总方案

按三个阶段推进，而不是一口气追求全自动。

---

## Phase 1：先止血，确保文本不丢

### 目标

在 Wayland 下，即使没有自动输入能力，也必须保证：

- 听写结果至少可靠进入剪贴板
- UI / 日志明确告诉用户当前走的是哪条 fallback
- 不再出现“代码认为成功，屏幕实际没字”的假成功状态

### 建议改动

#### 3.1 禁用 Wayland 下的“streaming insert 成功语义”

当前逻辑里，Linux 流式路径一旦 `type_unicode_chunk()` 返回成功，就会：

- 累积 `typed_text`
- 标记 `already_streamed=true`
- 跳过后续 inserter

这在 Wayland 下不可靠。

**建议：**
- 检测 `Linux + Wayland` 时，不让 `enigo.text(...)` 的返回值直接成为“已成功插入”的依据。
- Wayland 下默认不要走 `already_streamed=true` 的成功短路。

#### 3.2 Wayland 下默认降级为 copy-only

当前非流式路径是：

- 写入剪贴板
- 再用 `simulate_paste()` 发粘贴快捷键

Wayland 下第二步不可靠。

**建议：**
- 检测到 Wayland 时，默认走 **copy-only fallback**。
- 把文本留在剪贴板里，不要立即 restore。
- 明确给用户提示：`已复制到剪贴板，请手动粘贴`。

#### 3.3 把状态文案改成真话

需要避免如下误导：

- “已插入”但实际上没插入
- “已尝试粘贴”但用户无从判断文本是否已落到目标应用

**建议：**
- Wayland fallback 时统一使用明确状态：
  - `已复制到剪贴板，请手动粘贴`
  - `Wayland 当前未启用自动输入`
  - `剪贴板写入失败`

### Phase 1 接受标准

- Wayland 下听写后，文本不会 silently disappear。
- 即使自动输入失败，用户也总能从剪贴板找回文本。
- 日志和 UI 状态与真实行为一致。

---

## Phase 2：巩固当前 Wayland 触发路径

### 目标

把 Wayland 下已经落地的 `CLI + single-instance` 方案补齐到真正稳定、清晰、可交付，而不是在文档里把尚未验证的 portal 能力提前写成主路线。

### 建议改动

#### 3.4 明确把 CLI 路径当作当前正式支持方案

当前仓库已采用的路径是：

1. 启动时检测 Wayland session
2. 不安装 `rdev` 全局监听
3. 通过桌面环境快捷键执行：
   - `openless --toggle-dictation`
   - `openless --toggle-qa`
   - `openless --cancel-dictation`
4. 由 `tauri-plugin-single-instance` 把第二实例 argv 转发给主实例 coordinator

这里要做的不是推翻，而是补齐：

- Settings / README / Linux 指南里统一说明这是当前正式支持方式；
- 保证 GNOME / KDE / Hyprland / sway 等示例文案一致；
- 保证“有快捷键可触发”这件事在 Wayland 上可复现、可说明、可排障。

#### 3.5 portal 研究保留为后续增强方向

`xdg-desktop-portal` `GlobalShortcuts` 可以继续研究，但在仓库明确验证下面几点之前，不应写成主承诺：

- GNOME / KDE / 其他桌面上的真实可用范围
- 权限/交互模型是否符合产品心智
- 回退链路是否比当前 CLI 方案更简单而不是更碎

### 为什么这一层应该单独做

- 这是当前仓库已经落地的 Wayland 触发方案；
- 它能解决 #420 最核心的“如何触发听写”问题；
- 维护成本和跨桌面稳定性目前都优于贸然切 portal 主路线。

### Phase 2 接受标准

- Wayland 用户按文档/设置页说明配置后，能稳定触发 Dictation / QA / Cancel。
- 设置页、README、日志三处对 Wayland 触发方式的表述一致。
- 不把 `GlobalShortcuts portal` 写成已交付能力；如继续研究，应另开 research issue / PR。

---

## Phase 3：研究受权限控制的 Wayland 自动输入能力

### 目标

探索 Wayland 下真正的“自动把文本发到其他应用”能力，但只在 **有 compositor 支持 + 有用户授权** 的情况下启用。

### 候选路径

#### 3.5 `RemoteDesktop` portal + keyboard events

优点：
- 有官方 portal 文档
- 权限模型明确

缺点：
- 会话 / 授权交互更重
- 行为更像“远程控制权限”，不一定适合所有用户心智

#### 3.6 `RemoteDesktop` / `InputCapture` + `ConnectToEIS` + `libei`

优点：
- 这是 Wayland / compositor 体系里更现代的输入模拟路径
- 比直接赌 `enigo` / XTest 靠谱

缺点：
- 实现复杂度高
- compositor / backend 支持碎片化
- 仍然不是“全桌面无感通吃”的方案

#### 3.7 不建议把主方案押在 `virtual-keyboard-unstable-v1`

原因：
- 协议本身就标明不适合当通用稳定能力依赖
- compositor 是否开放给第三方应用不可控
- 产品层面碎片化风险太高

### Phase 3 的产品策略

自动输入必须是：

- **能力探测通过** 才启用
- **授权成功** 才启用
- 失败时明确回退到剪贴板方案

换句话说：

> Wayland 自动输入应该是“可选增强能力”，不是默认基本能力。

---

## 4. 对 #420 的建议拆单

建议把后续工作拆成三个 issue / PR 方向：

### 4.1 `wayland-output-safety`
范围：
- Wayland 下禁用假成功 streaming insert
- Wayland 下默认 copy-only
- 状态文案 / 日志对齐真实行为

这是最高优先级。

### 4.2 `wayland-trigger-path-hardening`
范围：
- 巩固 `CLI + single-instance` 触发链路
- Settings / README / Linux 文档统一
- GNOME / KDE / Hyprland / sway 示例与排障说明对齐

这是第二优先级。

### 4.3 `wayland-global-shortcuts-portal-research`
范围：
- 评估 `GlobalShortcuts` portal 的真实桌面支持面
- 验证是否值得从 research 升级为产品能力
- 只产出调研/原型，不提前改写当前支持承诺

这是后续研究方向，不应与当前可交付方案混写。

### 4.4 `wayland-hotkey-editor-flicker`
范围：
- 设置页快捷键录制时的闪烁 / 黑屏
- 只针对 UI / WebKitGTK / 输入录制链路处理

这个不要再跟“文本输出”绑一起看。

---

## 5. 我建议的实际落地顺序

### 第一刀（应先做）
- 修 `Wayland 文本输出不可靠`
- 核心目标：**不丢文本**

### 第二刀
- 巩固 `CLI + single-instance` 触发链路
- 核心目标：**让当前 Wayland 方案真正稳定、清晰、可交付**

### 第三刀
- 研究 `GlobalShortcuts portal` / `portal + libei` 能力
- 核心目标：**评估哪些能力值得升级成未来增强项**

### 第四刀
- 单独处理设置页闪烁 / 黑屏

---

## 6. 不建议做的事

### 6.1 不建议继续把 `enigo` 返回值当 Wayland 成功依据

因为这会继续制造：
- 日志成功
- UI 成功
- 用户实际没看到字

### 6.2 不建议把未验证的 portal 方案直接写成当前主实现

在仓库已经正式落地 CLI 路径的前提下，把 portal 提前写成“既定正路”，会让文档、代码与用户预期再次脱节。

### 6.3 不建议把 `virtual-keyboard-unstable-v1` 直接当主实现

它更像 compositor 特定能力，不适合直接做成发行版通用路径。

---

## 7. 结论

Wayland 下当然应该走一条“属于 Wayland 的路”，但这条路在当前仓库里应分成两层：

1. **当前正式触发路径** → `CLI + single-instance`
2. **剪贴板保底** → Wayland-native clipboard / copy-only fallback
3. **未来增强候选** → `GlobalShortcuts portal`、`RemoteDesktop` / `InputCapture` + `libei/EIS`（能力探测 + 用户授权）

如果只能先做一件事，优先级一定是：

> **先修文本输出链路，保证用户的话不会丢。**

---

## 8. 参考资料（用于后续实现，不是最终用户文案）

- XDG Portal GlobalShortcuts  
  https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.GlobalShortcuts.html
- XDG Portal RemoteDesktop  
  https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html
- XDG Portal InputCapture  
  https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.InputCapture.html
- XDG Portal Clipboard  
  https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Clipboard.html
- libei 文档  
  https://libinput.pages.freedesktop.org/libei/
- Wayland core / data transfer model  
  https://wayland.pages.freedesktop.org/wayland.freedesktop.org/docs/html/ch04.html
  https://wayland.freedesktop.org/docs/html/apa.html
