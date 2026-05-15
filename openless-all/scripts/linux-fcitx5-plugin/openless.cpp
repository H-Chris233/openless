/*
 * SPDX-FileCopyrightText: 2025 OpenLess Contributors
 *
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * fcitx5 插件 — 供 OpenLess 听写文字提交 + 快捷键监听。
 *
 * DBus 接口: org.fcitx.Fcitx.OpenLess1  (对象路径 /openless)
 *  方法:
 *    CommitText(s: text)     — 将文字提交到当前焦点输入上下文
 *    SetHotkey(as: keys)     — 设置听写触发快捷键 (Key::parse 格式)
 *    SetHotkeyRaw(uu: sym, states) — 直接设 sym+states (不走 parse)
 *  信号:
 *    DictationKeyEvent(uu: sym, states) — 热键被按下
 *
 *  后续: 当需要 IBus 引擎兼容时 (GNOME)，另行实现 org.freedesktop.IBus.Engine。
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
                        auto *ic = keyEvent.inputContext();
                        if (ic) {
                            savedIc_ = ic;
                        }
                    }
                    // 先检查 raw sym/states（修饰键专用路径，绕过 Key::parse 限制）
                    if ((triggerRawSym_ != 0 &&
                         keyEvent.key().sym() == static_cast<KeySym>(triggerRawSym_) &&
                         keyEvent.key().states() == static_cast<KeyStates>(triggerRawStates_)) ||
                        (triggerRawSym_ == 0 && [&]() {
                            for (const auto &hk : triggerKeyList_) {
                                if (keyEvent.key().sym() == hk.sym() &&
                                    keyEvent.key().states() == hk.states())
                                    return true;
                            }
                            return false;
                        }())) {
                        auto sym = triggerRawSym_ != 0
                            ? triggerRawSym_
                            : static_cast<uint32_t>(triggerKeyList_[0].sym());
                        auto states = triggerRawStates_ != 0
                            ? triggerRawStates_
                            : static_cast<uint32_t>(triggerKeyList_[0].states());
                        bool isPress = !keyEvent.isRelease();
                        FCITX_LOGC(openless, Debug)
                            << "Dictation hotkey: sym="
                            << sym << " states=" << states
                            << " isPress=" << isPress;
                        dictationKeyEvent(sym, states, isPress);
                        keyEvent.filterAndAccept();
                        return;
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

    FCITX_OBJECT_VTABLE_METHOD(commitText, "CommitText", "s", "");
    FCITX_OBJECT_VTABLE_METHOD(setHotkey, "SetHotkey", "as", "");
    FCITX_OBJECT_VTABLE_METHOD(setHotkeyRaw, "SetHotkeyRaw", "uu", "");
    FCITX_OBJECT_VTABLE_SIGNAL(dictationKeyEvent, "DictationKeyEvent", "uub");

    Instance *instance() { return instance_; }

    void reloadConfig() override {
        readAsIni(config_, configFile());
        // 加载原始 sym/states（由 SetHotkeyRaw 写入的持久化键值）
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
    /// 快捷键按下时保存的输入上下文指针，用于 commitText 在失焦后仍能提交文字。
    /// 事件处理线程和 DBus 处理线程都是 fcitx5 主事件循环，无竞态。
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
