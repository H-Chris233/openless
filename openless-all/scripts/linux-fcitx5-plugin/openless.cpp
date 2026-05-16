/*
 * SPDX-FileCopyrightText: 2025 OpenLess Contributors
 *
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * fcitx5 插件 — 供 OpenLess 听写文字提交 + 快捷键监听。
 *
 * DBus 接口: org.fcitx.Fcitx.OpenLess1  (对象路径 /openless)
 *  方法:
 *    CommitText(s: text)           — 将文字提交到当前焦点输入上下文
 *                                    安全性：本接口在会话总线(session bus)上对同用户
 *                                    所有进程开放，此为 fcitx5/IBus 体系的标准安全模型
 *                                    （非特权进程隔离）。
 *    SetHotkey(as: keys)           — 设置听写触发快捷键 (Key::parse 格式)
 *    SetHotkeyRaw(uu: sym, states) — 直接设听写触发 sym+states (不走 parse)
 *    SetCustomDictationTrigger(s: keyString) — 设置自定义组合键 (Key::parse 格式)
 *    SetQaHotkeyRaw(uu: sym, states)     — 直接设 QA 面板触发 sym+states
 *    SetTranslationHotkeyRaw(uu: sym, states) — 直接设翻译模式触发 sym+states
 *  信号:
 *    DictationKeyEvent(uub: sym, states, isPress) — 听写热键按下/抬起
 *    QaShortcutEvent(uub: sym, states, isPress)   — QA 快捷键按下/抬起
 *    TranslationModifierEvent(uub: sym, states, isPress) — 翻译修饰键按下/抬起
 */

#include <memory>
#include <string>
#include <vector>

#include <fcitx-config/configuration.h>
#include <fcitx-config/iniparser.h>
#include <fcitx-config/option.h>
#include <fcitx-utils/dbus/bus.h>
#include <fcitx-utils/dbus/objectvtable.h>
#include <fcitx-utils/handlertable.h>
#include <fcitx-utils/i18n.h>
#include <fcitx-utils/key.h>
#include <fcitx-utils/log.h>
#include <fcitx/addonfactory.h>
#include <fcitx/addoninstance.h>
#include <fcitx/addonmanager.h>
#include <fcitx/event.h>
#include <fcitx/inputcontext.h>
#include <fcitx/inputcontextmanager.h>
#include <fcitx/instance.h>
#include <fcitx-module/dbus/dbus_public.h>

FCITX_DEFINE_LOG_CATEGORY(openless, "openless");

namespace fcitx {

FCITX_CONFIGURATION(OpenLessConfig,
    KeyListOption triggerKey{this,
        "TriggerKey",
        _("Dictation trigger key"),
        {},
        KeyListConstrain()};
);

class OpenLess final : public AddonInstance,
                       public dbus::ObjectVTable<OpenLess> {
public:
    OpenLess(Instance *instance)
        : instance_(instance),
          triggerRawSym_(0),
          triggerRawStates_(0),
          qaRawSym_(0),
          qaRawStates_(0),
          translationRawSym_(0),
          translationRawStates_(0),
          hasCustomDictationKey_(false),
          savedIc_(nullptr) {

        // 1. 读取配置
        reloadConfig();

        // 2. 注册 DBus 接口
        auto *dbusMod = instance_->addonManager().addon("dbus", true);
        if (dbusMod) {
            auto *bus = dbusMod->call<IDBusModule::bus>();
            if (bus) {
                bus->addObjectVTable(
                    "/openless",
                    "org.fcitx.Fcitx.OpenLess1",
                    *this);
                FCITX_LOGC(openless, Info)
                    << "DBus interface registered at /openless";
            } else {
                FCITX_LOGC(openless, Warn)
                    << "Failed to get DBus bus";
            }
        } else {
            FCITX_LOGC(openless, Warn)
                << "DBus module not available";
        }

        // 3. 注册快捷键事件监听
        eventHandlers_.push_back(
            instance_->watchEvent(
                EventType::InputContextKeyEvent,
                EventWatcherPhase::PreInputMethod,
                [this](Event &event) {
                    auto &keyEvent = static_cast<KeyEvent &>(event);
                    // 保存当前输入上下文：快捷键按下时用户在目标 app 中，
                    // 此后胶囊窗口可能抢走焦点，但 commitText 仍能用此 IC 提交文字。
                    if (!keyEvent.isRelease()) {
                        savedIc_ = keyEvent.inputContext();
                    }

                    auto sym = static_cast<uint32_t>(keyEvent.key().sym());
                    auto states = static_cast<uint32_t>(keyEvent.key().states());
                    bool isPress = !keyEvent.isRelease();

                    // 检查自定义组合键（优先级最高）
                    if (hasCustomDictationKey_ &&
                        keyEvent.key().sym() == customDictationKey_.sym() &&
                        keyEvent.key().states() == customDictationKey_.states()) {
                        FCITX_LOGC(openless, Debug)
                            << "Custom dictation combo: sym=" << sym
                            << " states=" << states
                            << " isPress=" << isPress;
                        dictationKeyEvent(
                            static_cast<uint32_t>(customDictationKey_.sym()),
                            static_cast<uint32_t>(customDictationKey_.states()),
                            isPress);
                        keyEvent.filterAndAccept();
                        return;
                    }

                    // 检查听写触发键（raw + keylist 双路径）
                    if ((triggerRawSym_ != 0 &&
                         sym == triggerRawSym_ &&
                         states == triggerRawStates_) ||
                        (triggerRawSym_ == 0 && [&]() {
                            for (const auto &hk : triggerKeyList_) {
                                if (sym == static_cast<uint32_t>(hk.sym()) &&
                                    states == static_cast<uint32_t>(hk.states()))
                                    return true;
                            }
                            return false;
                        }())) {
                        auto dsym = triggerRawSym_ != 0
                            ? triggerRawSym_
                            : static_cast<uint32_t>(triggerKeyList_[0].sym());
                        auto dstates = triggerRawStates_ != 0
                            ? triggerRawStates_
                            : static_cast<uint32_t>(triggerKeyList_[0].states());
                        FCITX_LOGC(openless, Debug)
                            << "Dictation hotkey: sym=" << dsym
                            << " states=" << dstates
                            << " isPress=" << isPress;
                        dictationKeyEvent(dsym, dstates, isPress);
                        keyEvent.filterAndAccept();
                        return;
                    }

                    // 检查 QA 快捷键
                    if (qaRawSym_ != 0 &&
                        sym == qaRawSym_ &&
                        states == qaRawStates_) {
                        FCITX_LOGC(openless, Debug)
                            << "QA shortcut: sym=" << qaRawSym_
                            << " states=" << qaRawStates_
                            << " isPress=" << isPress;
                        qaShortcutEvent(qaRawSym_, qaRawStates_, isPress);
                        keyEvent.filterAndAccept();
                        return;
                    }

                    // 检查翻译模式修饰键（自定义 + 内置 Shift）
                    bool translationMatched = false;
                    if (translationRawSym_ != 0 &&
                        sym == translationRawSym_ &&
                        states == translationRawStates_) {
                        translationMatched = true;
                    }
                    // 内置 Shift 修饰键
                    if (sym == 0xffe1 || sym == 0xffe2) {
                        translationMatched = true;
                    }
                    if (translationMatched) {
                        FCITX_LOGC(openless, Debug)
                            << "Translation modifier: sym=" << sym
                            << " states=" << states
                            << " isPress=" << isPress;
                        translationModifierEvent(sym, states, isPress);
                        keyEvent.filterAndAccept();
                        return;
                    }
                }));

        // 4. 监听 InputContext 销毁事件，自动清空 savedIc_ 避免野指针
        eventHandlers_.push_back(
            instance_->watchEvent(
                EventType::InputContextDestroyed,
                EventWatcherPhase::Default,
                [this](Event &event) {
                    auto &icEvent = static_cast<InputContextEvent &>(event);
                    if (icEvent.inputContext() == savedIc_) {
                        savedIc_ = nullptr;
                    }
                }));

        FCITX_LOGC(openless, Info) << "OpenLess plugin loaded";
    }

    ~OpenLess() = default;

    // ---- DBus 方法 ----
    // 返回 void 而非 std::tuple<>，以匹配 FCITX_OBJECT_VTABLE_METHOD 的 RET("")

    void commitText(const std::string &text) {
        // 优先使用快捷键按下时保存的输入上下文（savedIc_），
        // 此时用户在目标 app 中，此后胶囊窗口抢焦点不影响提交。
        // 若 savedIc_ 为空则兜底用 foreachFocused。
        auto *ic = savedIc_;
        if (!ic) {
            FCITX_LOGC(openless, Warn)
                << "CommitText: savedIc_ is null, trying foreachFocused";
            auto &mgr = instance_->inputContextManager();
            mgr.foreachFocused([&](InputContext *focusedIc) {
                ic = focusedIc;
                return false;
            });
        }
        if (!ic) {
            FCITX_LOGC(openless, Warn)
                << "CommitText: no input context available";
            throw std::runtime_error("no focused input context");
        }
        FCITX_LOGC(openless, Debug) << "CommitText: " << text;
        ic->commitString(text);
    }

    void setHotkey(const std::vector<std::string> &keys) {
        KeyList keyList;
        for (const auto &s : keys) {
            Key key(s);
            if (key.isValid()) {
                keyList.push_back(key);
            } else {
                FCITX_LOGC(openless, Warn)
                    << "SetHotkey: invalid key '" << s << "'";
            }
        }
        config_.triggerKey.setValue(keyList);
        // KeyList 路径激活时清空 raw 路径，避免优先级冲突
        triggerRawSym_ = 0;
        triggerRawStates_ = 0;
        safeSaveAsIni(config_, configFile());
        // 同时清除磁盘上残留的 TriggerRawSym/TriggerRawStates（旧 raw 模式的持久化值），
        // 防止下次 fcitx5 重启 reloadConfig 重新加载旧 raw 热键覆盖新配置。
        {
            RawConfig raw;
            readAsIni(raw, configFile());
            raw.setValueByPath("TriggerRawSym", "0");
            raw.setValueByPath("TriggerRawStates", "0");
            safeSaveAsIni(raw, configFile());
        }
        rebuildTriggerKeys();
    }

    void setHotkeyRaw(uint32_t sym, uint32_t states) {
        triggerRawSym_ = sym;
        triggerRawStates_ = states;
        // 同时尝试维护 KeyList（如果 sym 可转为有效 key）
        Key key(static_cast<KeySym>(sym),
                static_cast<KeyStates>(states));
        if (key.isValid()) {
            KeyList keys = {key};
            config_.triggerKey.setValue(keys);
        } else {
            // 修饰键无法用 KeyList 表达，清空 KeyList 避免误匹配
            config_.triggerKey.setValue(KeyList{});
        }
        // 合并写入 config 和 raw sym/states
        RawConfig raw;
        raw.setValueByPath("TriggerRawSym", std::to_string(sym));
        raw.setValueByPath("TriggerRawStates", std::to_string(states));
        config_.save(raw);
        safeSaveAsIni(raw, configFile());
        rebuildTriggerKeys();
    }

    void setCustomDictationTrigger(const std::string &keyString) {
        Key key(keyString);
        if (!key.isValid()) {
            FCITX_LOGC(openless, Warn)
                << "SetCustomDictationTrigger: invalid key '" << keyString << "'";
            hasCustomDictationKey_ = false;
            return;
        }
        customDictationKey_ = key;
        hasCustomDictationKey_ = true;
        // 有自定义键时清空已有 raw+keylist 路径，避免双发
        triggerRawSym_ = 0;
        triggerRawStates_ = 0;
        config_.triggerKey.setValue(KeyList{});
        safeSaveAsIni(config_, configFile());
        FCITX_LOGC(openless, Info)
            << "SetCustomDictationTrigger: '" << keyString << "'"
            << " sym=" << static_cast<uint32_t>(key.sym())
            << " states=" << static_cast<uint32_t>(key.states());
    }

    void setQaHotkeyRaw(uint32_t sym, uint32_t states) {
        qaRawSym_ = sym;
        qaRawStates_ = states;
        RawConfig raw;
        readAsIni(raw, configFile());
        raw.setValueByPath("QaRawSym", std::to_string(sym));
        raw.setValueByPath("QaRawStates", std::to_string(states));
        safeSaveAsIni(raw, configFile());
        FCITX_LOGC(openless, Info)
            << "SetQaHotkeyRaw: sym=" << sym << " states=" << states;
    }

    void setTranslationHotkeyRaw(uint32_t sym, uint32_t states) {
        translationRawSym_ = sym;
        translationRawStates_ = states;
        RawConfig raw;
        readAsIni(raw, configFile());
        raw.setValueByPath("TranslationRawSym", std::to_string(sym));
        raw.setValueByPath("TranslationRawStates", std::to_string(states));
        safeSaveAsIni(raw, configFile());
        FCITX_LOGC(openless, Info)
            << "SetTranslationHotkeyRaw: sym=" << sym << " states=" << states;
    }

    FCITX_OBJECT_VTABLE_METHOD(commitText, "CommitText", "s", "");
    FCITX_OBJECT_VTABLE_METHOD(setHotkey, "SetHotkey", "as", "");
    FCITX_OBJECT_VTABLE_METHOD(setHotkeyRaw, "SetHotkeyRaw", "uu", "");
    FCITX_OBJECT_VTABLE_METHOD(setCustomDictationTrigger, "SetCustomDictationTrigger", "s", "");
    FCITX_OBJECT_VTABLE_METHOD(setQaHotkeyRaw, "SetQaHotkeyRaw", "uu", "");
    FCITX_OBJECT_VTABLE_METHOD(setTranslationHotkeyRaw, "SetTranslationHotkeyRaw", "uu", "");
    FCITX_OBJECT_VTABLE_SIGNAL(dictationKeyEvent, "DictationKeyEvent", "uub");
    FCITX_OBJECT_VTABLE_SIGNAL(qaShortcutEvent, "QaShortcutEvent", "uub");
    FCITX_OBJECT_VTABLE_SIGNAL(translationModifierEvent, "TranslationModifierEvent", "uub");

    Instance *instance() { return instance_; }

    void reloadConfig() override {
        readAsIni(config_, configFile());
        // 加载原始 sym/states（由 SetHotkeyRaw / SetQaHotkeyRaw / SetTranslationHotkeyRaw 写入的持久化键值）
        RawConfig raw;
        readAsIni(raw, configFile());
        {
            auto *v = raw.valueByPath("TriggerRawSym");
            triggerRawSym_ = v ? std::stoul(*v, nullptr, 0) : 0;
        }
        {
            auto *v = raw.valueByPath("TriggerRawStates");
            triggerRawStates_ = v ? std::stoul(*v, nullptr, 0) : 0;
        }
        {
            auto *v = raw.valueByPath("QaRawSym");
            qaRawSym_ = v ? std::stoul(*v, nullptr, 0) : 0;
        }
        {
            auto *v = raw.valueByPath("QaRawStates");
            qaRawStates_ = v ? std::stoul(*v, nullptr, 0) : 0;
        }
        {
            auto *v = raw.valueByPath("TranslationRawSym");
            translationRawSym_ = v ? std::stoul(*v, nullptr, 0) : 0;
        }
        {
            auto *v = raw.valueByPath("TranslationRawStates");
            translationRawStates_ = v ? std::stoul(*v, nullptr, 0) : 0;
        }
        rebuildTriggerKeys();
    }

    const Configuration *getConfig() const override {
        return &config_;
    }

    void setConfig(const RawConfig &rawConfig) override {
        config_.load(rawConfig, true);
        safeSaveAsIni(config_, configFile());
        rebuildTriggerKeys();
    }

private:
    static constexpr const char *configFile() {
        return "conf/openless.conf";
    }

    void rebuildTriggerKeys() {
        triggerKeyList_ = config_.triggerKey.value();
    }

    Instance *instance_;
    OpenLessConfig config_;
    KeyList triggerKeyList_;
    uint32_t triggerRawSym_;
    uint32_t triggerRawStates_;
    uint32_t qaRawSym_;
    uint32_t qaRawStates_;
    uint32_t translationRawSym_;
    uint32_t translationRawStates_;
    Key customDictationKey_;
    bool hasCustomDictationKey_;
    /// 快捷键按下时保存的输入上下文指针，用于 commitText 在失焦后仍能提交文字。
    /// 事件处理线程和 DBus 处理线程都是 fcitx5 主事件循环，无竞态。
    /// 通过 InputContextDestroyed 事件监听 IC 销毁时自动清空指针。
    InputContext *savedIc_;
    std::vector<std::unique_ptr<HandlerTableEntry<EventHandler>>>
        eventHandlers_;
};

class OpenLessFactory : public AddonFactory {
public:
    AddonInstance *create(AddonManager *manager) override {
        return new OpenLess(manager->instance());
    }
};

} // namespace fcitx

FCITX_ADDON_FACTORY(fcitx::OpenLessFactory);
