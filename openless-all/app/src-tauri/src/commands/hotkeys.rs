use super::*;

#[tauri::command]
pub fn validate_shortcut_binding(binding: ShortcutBinding) -> Result<(), String> {
    crate::shortcut_binding::validate_binding(&binding).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_dictation_hotkey(
    coord: CoordinatorState<'_>,
    binding: ShortcutBinding,
) -> Result<(), String> {
    crate::shortcut_binding::validate_binding(&binding).map_err(|e| e.to_string())?;
    reject_bare_shift_dictation_shortcut(&binding)?;
    let mut prefs = coord.prefs().get();
    if let Some(qa_hotkey) = prefs.qa_hotkey.as_ref() {
        reject_dictation_qa_hotkey_overlap(&binding, qa_hotkey)?;
    }
    reject_dictation_translation_hotkey_overlap(&binding, &prefs.translation_hotkey)?;
    if let Some(switch_style) = prefs.switch_style_hotkey.as_ref() {
        reject_dictation_switch_style_hotkey_overlap(&binding, switch_style)?;
    }
    if let Some(open_app) = prefs.open_app_hotkey.as_ref() {
        reject_dictation_open_app_hotkey_overlap(&binding, open_app)?;
    }
    prefs.dictation_hotkey = binding;
    sync_dictation_hotkey_legacy_fields(&mut prefs);
    coord.prefs().set(prefs).map_err(|e| e.to_string())?;
    coord.update_hotkey_binding();
    coord.update_combo_hotkey_binding();
    Ok(())
}

#[tauri::command]
pub fn set_translation_hotkey(
    coord: CoordinatorState<'_>,
    binding: ShortcutBinding,
) -> Result<(), String> {
    crate::shortcut_binding::validate_binding(&binding).map_err(|e| e.to_string())?;
    let previous = coord.prefs().get();
    reject_dictation_translation_hotkey_overlap(&previous.dictation_hotkey, &binding)?;
    if let Some(qa_hotkey) = previous.qa_hotkey.as_ref() {
        reject_qa_translation_hotkey_overlap(qa_hotkey, &binding)?;
    }
    if let Some(switch_style) = previous.switch_style_hotkey.as_ref() {
        reject_translation_switch_style_hotkey_overlap(&binding, switch_style)?;
    }
    if let Some(open_app) = previous.open_app_hotkey.as_ref() {
        reject_translation_open_app_hotkey_overlap(&binding, open_app)?;
    }
    let mut prefs = previous.clone();
    prefs.translation_hotkey = binding;
    coord.prefs().set(prefs).map_err(|e| e.to_string())?;
    if let Err(e) = coord.try_update_translation_hotkey_binding() {
        if let Err(rollback_err) = coord.prefs().set(previous) {
            log::warn!("[commands] 回滚翻译快捷键失败: {rollback_err}");
        }
        coord.update_translation_hotkey_binding();
        return Err(e);
    }
    Ok(())
}

/// 设置「切换风格」全局快捷键。`binding == None`（前端传 null）= 停用：清空绑定并
/// 反注册全局键。镜像 `set_qa_hotkey` 的 `Option=None` 停用模式（issue #576）。
#[tauri::command]
pub fn set_switch_style_hotkey(
    coord: CoordinatorState<'_>,
    binding: Option<ShortcutBinding>,
) -> Result<(), String> {
    if let Some(binding) = binding.as_ref() {
        crate::shortcut_binding::validate_binding(binding).map_err(|e| e.to_string())?;
        reject_modifier_only_action_shortcut(binding)?;
    }
    let mut prefs = coord.prefs().get();
    if let Some(binding) = binding.as_ref() {
        reject_dictation_switch_style_hotkey_overlap(&prefs.dictation_hotkey, binding)?;
        reject_translation_switch_style_hotkey_overlap(&prefs.translation_hotkey, binding)?;
        if let Some(qa_hotkey) = prefs.qa_hotkey.as_ref() {
            reject_qa_switch_style_hotkey_overlap(qa_hotkey, binding)?;
        }
        if let Some(open_app) = prefs.open_app_hotkey.as_ref() {
            reject_switch_style_open_app_hotkey_overlap(binding, open_app)?;
        }
    }
    prefs.switch_style_hotkey = binding;
    coord.prefs().set(prefs).map_err(|e| e.to_string())?;
    coord.update_switch_style_hotkey_binding();
    Ok(())
}

/// 设置「唤起 App」全局快捷键。`binding == None`（前端传 null）= 停用（同上）。
#[tauri::command]
pub fn set_open_app_hotkey(
    coord: CoordinatorState<'_>,
    binding: Option<ShortcutBinding>,
) -> Result<(), String> {
    if let Some(binding) = binding.as_ref() {
        crate::shortcut_binding::validate_binding(binding).map_err(|e| e.to_string())?;
        reject_modifier_only_action_shortcut(binding)?;
    }
    let mut prefs = coord.prefs().get();
    if let Some(binding) = binding.as_ref() {
        reject_dictation_open_app_hotkey_overlap(&prefs.dictation_hotkey, binding)?;
        reject_translation_open_app_hotkey_overlap(&prefs.translation_hotkey, binding)?;
        if let Some(qa_hotkey) = prefs.qa_hotkey.as_ref() {
            reject_qa_open_app_hotkey_overlap(qa_hotkey, binding)?;
        }
        if let Some(switch_style) = prefs.switch_style_hotkey.as_ref() {
            reject_switch_style_open_app_hotkey_overlap(switch_style, binding)?;
        }
    }
    prefs.open_app_hotkey = binding;
    coord.prefs().set(prefs).map_err(|e| e.to_string())?;
    coord.update_open_app_hotkey_binding();
    Ok(())
}

fn reject_modifier_only_action_shortcut(binding: &ShortcutBinding) -> Result<(), String> {
    if binding.modifiers.is_empty()
        && (binding.primary.eq_ignore_ascii_case("shift")
            || crate::shortcut_binding::legacy_modifier_trigger(binding).is_some())
    {
        return Err("该快捷键需要使用组合键或非修饰主键".into());
    }
    Ok(())
}

#[tauri::command]
pub fn validate_combo_hotkey(binding: ComboBinding) -> Result<(), String> {
    let shortcut = ShortcutBinding {
        primary: binding.primary,
        modifiers: binding.modifiers,
    };
    reject_bare_shift_dictation_shortcut(&shortcut)?;
    crate::combo_hotkey::validate_binding(&shortcut).map_err(|e| e.to_string())
}

/// 设置自定义录音组合键并热更新 monitor。
#[tauri::command]
pub fn set_combo_hotkey(coord: CoordinatorState<'_>, binding: ComboBinding) -> Result<(), String> {
    let mut prefs = coord.prefs().get();
    let shortcut = ShortcutBinding {
        primary: binding.primary.clone(),
        modifiers: binding.modifiers.clone(),
    };
    reject_bare_shift_dictation_shortcut(&shortcut)?;
    crate::combo_hotkey::validate_binding(&shortcut).map_err(|e| e.to_string())?;
    if let Some(qa_hotkey) = prefs.qa_hotkey.as_ref() {
        reject_dictation_qa_hotkey_overlap(&shortcut, qa_hotkey)?;
    }
    reject_dictation_translation_hotkey_overlap(&shortcut, &prefs.translation_hotkey)?;
    if let Some(switch_style) = prefs.switch_style_hotkey.as_ref() {
        reject_dictation_switch_style_hotkey_overlap(&shortcut, switch_style)?;
    }
    if let Some(open_app) = prefs.open_app_hotkey.as_ref() {
        reject_dictation_open_app_hotkey_overlap(&shortcut, open_app)?;
    }
    prefs.custom_combo_hotkey = Some(binding);
    prefs.dictation_hotkey = shortcut;
    sync_dictation_hotkey_legacy_fields(&mut prefs);
    coord.prefs().set(prefs).map_err(|e| e.to_string())?;
    coord.update_hotkey_binding();
    coord.update_combo_hotkey_binding();
    Ok(())
}

pub(crate) fn reject_bare_shift_dictation_shortcut(
    binding: &ShortcutBinding,
) -> Result<(), String> {
    if binding.modifiers.is_empty() && binding.primary.eq_ignore_ascii_case("shift") {
        return Err("Shift 单键目前只能用于翻译快捷键".into());
    }
    Ok(())
}

pub(crate) fn sync_dictation_hotkey_legacy_fields(prefs: &mut UserPreferences) {
    if let Some(trigger) = crate::shortcut_binding::legacy_modifier_trigger(&prefs.dictation_hotkey)
    {
        prefs.hotkey.trigger = trigger;
        prefs.custom_combo_hotkey = None;
        return;
    }
    prefs.hotkey.trigger = crate::types::HotkeyTrigger::Custom;
    prefs.custom_combo_hotkey = if prefs.dictation_hotkey.primary.trim().is_empty() {
        None
    } else {
        Some(ComboBinding {
            primary: prefs.dictation_hotkey.primary.clone(),
            modifiers: prefs.dictation_hotkey.modifiers.clone(),
        })
    };
}

pub(crate) fn reject_dictation_qa_hotkey_overlap(
    dictation: &ShortcutBinding,
    qa: &ShortcutBinding,
) -> Result<(), String> {
    if shortcut_bindings_overlap(dictation, qa) {
        return Err("QA 快捷键不能和听写快捷键相同".into());
    }
    Ok(())
}

fn reject_hotkey_overlap(
    left: &ShortcutBinding,
    right: &ShortcutBinding,
    message: &'static str,
) -> Result<(), String> {
    if shortcut_bindings_overlap(left, right) {
        return Err(message.into());
    }
    Ok(())
}

pub(crate) fn reject_hotkey_collisions(prefs: &UserPreferences) -> Result<(), String> {
    // 停用（None）的 action 快捷键不参与任何冲突检测。
    let switch_style = prefs.switch_style_hotkey.as_ref();
    let open_app = prefs.open_app_hotkey.as_ref();
    let less_computer = prefs.coding_agent_voice_hotkey.as_ref();
    if let Some(qa_hotkey) = prefs.qa_hotkey.as_ref() {
        reject_dictation_qa_hotkey_overlap(&prefs.dictation_hotkey, qa_hotkey)?;
        reject_qa_translation_hotkey_overlap(qa_hotkey, &prefs.translation_hotkey)?;
        if let Some(less_computer) = less_computer {
            reject_qa_less_computer_hotkey_overlap(qa_hotkey, less_computer)?;
        }
        if let Some(switch_style) = switch_style {
            reject_qa_switch_style_hotkey_overlap(qa_hotkey, switch_style)?;
        }
        if let Some(open_app) = open_app {
            reject_qa_open_app_hotkey_overlap(qa_hotkey, open_app)?;
        }
    }
    reject_dictation_translation_hotkey_overlap(
        &prefs.dictation_hotkey,
        &prefs.translation_hotkey,
    )?;
    if let Some(less_computer) = less_computer {
        reject_dictation_less_computer_hotkey_overlap(&prefs.dictation_hotkey, less_computer)?;
        reject_translation_less_computer_hotkey_overlap(&prefs.translation_hotkey, less_computer)?;
    }
    if let Some(switch_style) = switch_style {
        reject_dictation_switch_style_hotkey_overlap(&prefs.dictation_hotkey, switch_style)?;
        reject_translation_switch_style_hotkey_overlap(&prefs.translation_hotkey, switch_style)?;
        if let Some(less_computer) = less_computer {
            reject_less_computer_switch_style_hotkey_overlap(less_computer, switch_style)?;
        }
    }
    if let Some(open_app) = open_app {
        reject_dictation_open_app_hotkey_overlap(&prefs.dictation_hotkey, open_app)?;
        reject_translation_open_app_hotkey_overlap(&prefs.translation_hotkey, open_app)?;
        if let Some(less_computer) = less_computer {
            reject_less_computer_open_app_hotkey_overlap(less_computer, open_app)?;
        }
    }
    if let (Some(switch_style), Some(open_app)) = (switch_style, open_app) {
        reject_switch_style_open_app_hotkey_overlap(switch_style, open_app)?;
    }
    Ok(())
}

pub(crate) fn reject_dictation_translation_hotkey_overlap(
    dictation: &ShortcutBinding,
    translation: &ShortcutBinding,
) -> Result<(), String> {
    reject_hotkey_overlap(dictation, translation, "翻译快捷键不能和听写快捷键相同")
}

fn reject_dictation_switch_style_hotkey_overlap(
    dictation: &ShortcutBinding,
    switch_style: &ShortcutBinding,
) -> Result<(), String> {
    reject_hotkey_overlap(
        dictation,
        switch_style,
        "切换风格快捷键不能和听写快捷键相同",
    )
}

fn reject_dictation_open_app_hotkey_overlap(
    dictation: &ShortcutBinding,
    open_app: &ShortcutBinding,
) -> Result<(), String> {
    reject_hotkey_overlap(dictation, open_app, "打开应用快捷键不能和听写快捷键相同")
}

fn reject_dictation_less_computer_hotkey_overlap(
    dictation: &ShortcutBinding,
    less_computer: &ShortcutBinding,
) -> Result<(), String> {
    reject_hotkey_overlap(
        dictation,
        less_computer,
        "Less Computer 快捷键不能和听写快捷键相同",
    )
}

pub(crate) fn reject_qa_translation_hotkey_overlap(
    qa: &ShortcutBinding,
    translation: &ShortcutBinding,
) -> Result<(), String> {
    reject_hotkey_overlap(qa, translation, "翻译快捷键不能和 QA 快捷键相同")
}

pub(crate) fn reject_qa_switch_style_hotkey_overlap(
    qa: &ShortcutBinding,
    switch_style: &ShortcutBinding,
) -> Result<(), String> {
    reject_hotkey_overlap(qa, switch_style, "切换风格快捷键不能和 QA 快捷键相同")
}

pub(crate) fn reject_qa_open_app_hotkey_overlap(
    qa: &ShortcutBinding,
    open_app: &ShortcutBinding,
) -> Result<(), String> {
    reject_hotkey_overlap(qa, open_app, "打开应用快捷键不能和 QA 快捷键相同")
}

fn reject_qa_less_computer_hotkey_overlap(
    qa: &ShortcutBinding,
    less_computer: &ShortcutBinding,
) -> Result<(), String> {
    reject_hotkey_overlap(
        qa,
        less_computer,
        "Less Computer 快捷键不能和 QA 快捷键相同",
    )
}

fn reject_translation_switch_style_hotkey_overlap(
    translation: &ShortcutBinding,
    switch_style: &ShortcutBinding,
) -> Result<(), String> {
    reject_hotkey_overlap(
        translation,
        switch_style,
        "切换风格快捷键不能和翻译快捷键相同",
    )
}

fn reject_translation_open_app_hotkey_overlap(
    translation: &ShortcutBinding,
    open_app: &ShortcutBinding,
) -> Result<(), String> {
    reject_hotkey_overlap(translation, open_app, "打开应用快捷键不能和翻译快捷键相同")
}

fn reject_translation_less_computer_hotkey_overlap(
    translation: &ShortcutBinding,
    less_computer: &ShortcutBinding,
) -> Result<(), String> {
    reject_hotkey_overlap(
        translation,
        less_computer,
        "Less Computer 快捷键不能和翻译快捷键相同",
    )
}

fn reject_switch_style_open_app_hotkey_overlap(
    switch_style: &ShortcutBinding,
    open_app: &ShortcutBinding,
) -> Result<(), String> {
    reject_hotkey_overlap(
        switch_style,
        open_app,
        "打开应用快捷键不能和切换风格快捷键相同",
    )
}

fn reject_less_computer_switch_style_hotkey_overlap(
    less_computer: &ShortcutBinding,
    switch_style: &ShortcutBinding,
) -> Result<(), String> {
    reject_hotkey_overlap(
        less_computer,
        switch_style,
        "Less Computer 快捷键不能和切换风格快捷键相同",
    )
}

fn reject_less_computer_open_app_hotkey_overlap(
    less_computer: &ShortcutBinding,
    open_app: &ShortcutBinding,
) -> Result<(), String> {
    reject_hotkey_overlap(
        less_computer,
        open_app,
        "Less Computer 快捷键不能和打开应用快捷键相同",
    )
}

fn shortcut_bindings_overlap(left: &ShortcutBinding, right: &ShortcutBinding) -> bool {
    let left_legacy = crate::shortcut_binding::legacy_modifier_trigger(left);
    let right_legacy = crate::shortcut_binding::legacy_modifier_trigger(right);
    match (left_legacy, right_legacy) {
        (Some(left), Some(right)) => left == right,
        (Some(_), None) | (None, Some(_)) => false,
        (None, None) => {
            let Ok(left) = crate::shortcut_binding::parse_global_hotkey(left) else {
                return false;
            };
            let Ok(right) = crate::shortcut_binding::parse_global_hotkey(right) else {
                return false;
            };
            left == right
        }
    }
}
