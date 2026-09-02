#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("openless-linux-egui is only available on Linux");
}

#[cfg(target_os = "linux")]
mod linux_app {
    use std::future::Future;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    use eframe::egui;
    use openless_core::{
        BackendConfig, BackendError, BackendEvent, BackendEventKind, BackendSnapshot,
        DictationPhase, HistoryInsertStatus, HostAction, LessComputerEventKind, LocalAsrModel,
        LocalAsrRuntime, UserPreferences,
    };
    use openless_linux_egui::{
        drain_events, Fcitx5HotkeyListener, LinuxBackendBuilder, LinuxCapabilitySnapshot,
        LinuxLaunchIntent, LinuxNativeRuntime, LinuxPackageKind, SingleInstanceBroker,
        SingleInstanceRole,
    };

    enum UiResult {
        Message(String),
        Models(Result<Vec<LocalAsrModel>, String>),
    }

    #[derive(Clone)]
    enum ModelsState {
        Loading,
        Loaded(Vec<LocalAsrModel>),
        Failed(String),
    }

    pub struct OpenLessEguiApp {
        tokio: Arc<tokio::runtime::Runtime>,
        native: Option<LinuxNativeRuntime>,
        subscription: Option<openless_core::EventSubscription>,
        snapshot: Option<BackendSnapshot>,
        preferences: Option<UserPreferences>,
        models: ModelsState,
        transcript: String,
        less_computer_input: String,
        less_computer_output: String,
        pending_approval: Option<(String, String)>,
        status: String,
        startup_error: Option<String>,
        tx: mpsc::Sender<UiResult>,
        rx: mpsc::Receiver<UiResult>,
    }

    impl OpenLessEguiApp {
        fn new(
            tokio: Arc<tokio::runtime::Runtime>,
            native: Result<LinuxNativeRuntime, String>,
        ) -> Self {
            let (tx, rx) = mpsc::channel();
            match native {
                Ok(native) => {
                    let backend = native.host().backend();
                    let snapshot = backend.snapshot();
                    let preferences = backend.get_preferences();
                    let subscription = backend.subscribe();
                    let app = Self {
                        tokio,
                        native: Some(native),
                        subscription: Some(subscription),
                        snapshot: Some(snapshot),
                        preferences: Some(preferences),
                        models: ModelsState::Loading,
                        transcript: String::new(),
                        less_computer_input: String::new(),
                        less_computer_output: String::new(),
                        pending_approval: None,
                        status: "Core 2.0 已启动".to_string(),
                        startup_error: None,
                        tx,
                        rx,
                    };
                    app.load_models();
                    app
                }
                Err(error) => Self {
                    tokio,
                    native: None,
                    subscription: None,
                    snapshot: None,
                    preferences: None,
                    models: ModelsState::Loading,
                    transcript: String::new(),
                    less_computer_input: String::new(),
                    less_computer_output: String::new(),
                    pending_approval: None,
                    status: "启动失败".to_string(),
                    startup_error: Some(error),
                    tx,
                    rx,
                },
            }
        }

        fn backend(&self) -> Option<Arc<openless_core::OpenLessBackend>> {
            self.native
                .as_ref()
                .map(|native| Arc::clone(native.host().backend()))
        }

        fn spawn<F>(&self, future: F)
        where
            F: Future<Output = Result<String, BackendError>> + Send + 'static,
        {
            let tx = self.tx.clone();
            self.tokio.spawn(async move {
                let message = future.await.unwrap_or_else(|error| error.to_string());
                let _ = tx.send(UiResult::Message(message));
            });
        }

        fn load_models(&self) {
            let Some(backend) = self.backend() else {
                return;
            };
            let tx = self.tx.clone();
            self.tokio.spawn(async move {
                let models = backend
                    .services()
                    .local_asr
                    .list_models(LocalAsrRuntime::Generic)
                    .await
                    .map_err(|error| error.to_string());
                let _ = tx.send(UiResult::Models(models));
            });
        }

        fn apply_event(&mut self, event: BackendEvent) {
            match event.kind {
                BackendEventKind::DictationStateChanged(state) => {
                    self.status = format!("听写：{:?}", state.phase);
                }
                BackendEventKind::TranscriptDelta(delta) => self.transcript.push_str(&delta.text),
                BackendEventKind::PolishDelta(delta) if delta.is_final => {
                    self.transcript = delta.text;
                }
                BackendEventKind::DictationCompleted(result) => {
                    self.transcript = result.polished_text;
                    self.status = format!("听写完成：{:?}", result.inserted);
                }
                BackendEventKind::LessComputerEvent(event) => match event.kind {
                    LessComputerEventKind::Delta { text } => {
                        self.less_computer_output.push_str(&text);
                    }
                    LessComputerEventKind::Completed { text, .. } => {
                        if self.less_computer_output.is_empty() {
                            self.less_computer_output = text;
                        }
                        self.pending_approval = None;
                    }
                    LessComputerEventKind::Approval { token, command, .. } => {
                        self.pending_approval = Some((token, command));
                    }
                    LessComputerEventKind::Error { message } => self.status = message,
                    LessComputerEventKind::Cancelled => {
                        self.status = "Less Computer 已取消".to_string()
                    }
                    _ => {}
                },
                BackendEventKind::LocalAsrDownloadProgress(progress) => {
                    self.status = format!(
                        "模型 {}：{:?} {}/{}",
                        progress.model_id,
                        progress.phase,
                        progress.bytes_downloaded,
                        progress.bytes_total
                    );
                    if matches!(
                        progress.phase,
                        openless_core::LocalAsrDownloadPhase::Finished
                            | openless_core::LocalAsrDownloadPhase::Failed
                            | openless_core::LocalAsrDownloadPhase::Cancelled
                    ) {
                        self.models = ModelsState::Loading;
                        self.load_models();
                    }
                }
                BackendEventKind::PreferencesChanged(_) => {
                    if let Some(backend) = self.backend() {
                        self.preferences = Some(backend.get_preferences());
                    }
                }
                _ => {}
            }
        }

        fn poll(&mut self, ctx: &egui::Context) {
            if let Some(native) = &self.native {
                let (launch_intents, hotkey_events, errors) = native.drain_native_events();
                let host = native.host_arc();
                for intent in launch_intents {
                    let host = Arc::clone(&host);
                    self.spawn(async move {
                        host.dispatch_launch_intent(intent).await?;
                        Ok("已处理启动请求".to_string())
                    });
                }
                for event in hotkey_events {
                    let host = Arc::clone(&host);
                    self.spawn(async move {
                        host.dispatch_hotkey_event(event).await?;
                        Ok("已处理快捷键".to_string())
                    });
                }
                if let Some(error) = errors.last() {
                    self.status = error.to_string();
                }

                let mut actions = Vec::new();
                native.host_actions().drain(|action| actions.push(action));
                for action in actions {
                    match action {
                        HostAction::ShowMain | HostAction::ShowLessComputer => {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        }
                        HostAction::FocusMain => {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                        }
                        HostAction::Notify(message) => self.status = message,
                        HostAction::OpenExternalUrl(url) | HostAction::OpenSystemSettings(url) => {
                            std::thread::spawn(move || {
                                let _ = std::process::Command::new("xdg-open").arg(url).status();
                            });
                        }
                        HostAction::RequestRestart => {
                            self.status = "请手动重启 OpenLess".to_string();
                        }
                        HostAction::ShowDictationFeedback
                        | HostAction::HideDictationFeedback
                        | HostAction::ShowSelectionPreview
                        | HostAction::HideSelectionPreview
                        | HostAction::ShowQa
                        | HostAction::HideQa => {}
                    }
                }
            }
            let mut events = Vec::new();
            if let Some(subscription) = self.subscription.as_mut() {
                let _ = drain_events(subscription, |event| events.push(event));
            }
            for event in events {
                self.apply_event(event);
            }
            while let Ok(result) = self.rx.try_recv() {
                match result {
                    UiResult::Message(message) => self.status = message,
                    UiResult::Models(Ok(models)) => self.models = ModelsState::Loaded(models),
                    UiResult::Models(Err(error)) => {
                        self.models = ModelsState::Failed(error.clone());
                        self.status = error;
                    }
                }
            }
            if let Some(backend) = self.backend() {
                self.snapshot = Some(backend.snapshot());
            }
        }

        fn dictation_ui(&mut self, ui: &mut egui::Ui) {
            ui.heading("听写");
            let phase = self
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.dictation.phase)
                .unwrap_or(DictationPhase::Idle);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(phase == DictationPhase::Idle, egui::Button::new("开始"))
                    .clicked()
                {
                    if let Some(backend) = self.backend() {
                        self.transcript.clear();
                        self.spawn(async move {
                            backend.start_dictation().await?;
                            Ok("正在录音".to_string())
                        });
                    }
                }
                if ui
                    .add_enabled(
                        phase == DictationPhase::Recording,
                        egui::Button::new("停止"),
                    )
                    .clicked()
                {
                    if let Some(backend) = self.backend() {
                        self.spawn(async move {
                            let result = backend.stop_dictation().await?;
                            Ok(format!("完成：{} 字", result.polished_text.chars().count()))
                        });
                    }
                }
                if ui
                    .add_enabled(phase != DictationPhase::Idle, egui::Button::new("取消"))
                    .clicked()
                {
                    if let Some(backend) = self.backend() {
                        self.spawn(async move {
                            backend.cancel_dictation(None).await?;
                            Ok("听写已取消".to_string())
                        });
                    }
                }
            });
            ui.label(if self.transcript.is_empty() {
                "尚无转写结果"
            } else {
                &self.transcript
            });
        }

        fn less_computer_ui(&mut self, ui: &mut egui::Ui) {
            ui.heading("Less Computer");
            ui.text_edit_multiline(&mut self.less_computer_input);
            ui.horizontal(|ui| {
                if ui.button("运行").clicked() && !self.less_computer_input.trim().is_empty() {
                    if let Some(backend) = self.backend() {
                        let prompt = self.less_computer_input.clone();
                        self.less_computer_output.clear();
                        self.spawn(async move {
                            backend.submit_less_computer(prompt).await?;
                            Ok("Less Computer 已完成".to_string())
                        });
                    }
                }
                if ui.button("取消").clicked() {
                    if let Some(backend) = self.backend() {
                        self.spawn(async move {
                            backend.cancel_less_computer(None).await?;
                            Ok("Less Computer 已取消".to_string())
                        });
                    }
                }
            });
            if let Some((token, command)) = self.pending_approval.clone() {
                ui.label(format!("请求执行：{command}"));
                ui.horizontal(|ui| {
                    for (label, approved) in [("允许", true), ("拒绝", false)] {
                        if ui.button(label).clicked() {
                            if let Some(backend) = self.backend() {
                                let token = token.clone();
                                self.pending_approval = None;
                                self.spawn(async move {
                                    backend
                                        .services()
                                        .less_computer
                                        .approve(token, approved)
                                        .await?;
                                    Ok("审批已提交".to_string())
                                });
                            }
                        }
                    }
                });
            }
            ui.label(if self.less_computer_output.is_empty() {
                "尚无 Agent 输出"
            } else {
                &self.less_computer_output
            });
        }

        fn models_ui(&mut self, ui: &mut egui::Ui) {
            ui.horizontal(|ui| {
                ui.heading("本地模型");
                if ui.button("刷新").clicked() {
                    self.models = ModelsState::Loading;
                    self.load_models();
                }
            });
            let models = match self.models.clone() {
                ModelsState::Loading => {
                    ui.label("正在加载模型目录…");
                    return;
                }
                ModelsState::Failed(error) => {
                    ui.colored_label(egui::Color32::RED, error);
                    return;
                }
                ModelsState::Loaded(models) if models.is_empty() => {
                    ui.label("模型目录未返回任何可用模型");
                    return;
                }
                ModelsState::Loaded(models) => models,
            };
            for model in models {
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "{} · {} · {}",
                        model.display_name,
                        model.family,
                        if model.installed {
                            "已安装"
                        } else {
                            "未安装"
                        }
                    ));
                    if !model.installed && ui.button("下载").clicked() {
                        if let Some(backend) = self.backend() {
                            let target = model.target.clone();
                            self.spawn(async move {
                                backend
                                    .services()
                                    .local_asr
                                    .start_download(target, None)
                                    .await?;
                                Ok("模型下载完成".to_string())
                            });
                        }
                    }
                    if ui.button("取消").clicked() {
                        if let Some(backend) = self.backend() {
                            let target = model.target.clone();
                            self.spawn(async move {
                                backend.services().local_asr.cancel_download(target).await?;
                                Ok("模型下载已取消".to_string())
                            });
                        }
                    }
                });
            }
        }

        fn settings_ui(&mut self, ui: &mut egui::Ui) {
            ui.heading("Provider 与设置");
            if let Some(snapshot) = &self.snapshot {
                let credentials = &snapshot.credentials;
                ui.label(format!(
                    "ASR：{}（{}）",
                    credentials.active_asr_provider,
                    if credentials.asr_configured {
                        "已配置"
                    } else {
                        "未配置"
                    }
                ));
                ui.label(format!(
                    "LLM：{}（{}）",
                    credentials.active_llm_provider,
                    if credentials.llm_configured {
                        "已配置"
                    } else {
                        "未配置"
                    }
                ));
            }
            if let Some(preferences) = self.preferences.as_mut() {
                ui.checkbox(&mut preferences.streaming_insert, "流式插入");
                ui.checkbox(&mut preferences.coding_agent_enabled, "启用 Less Computer");
                if ui.button("保存设置").clicked() {
                    if let (Some(native), Some(snapshot)) = (&self.native, &self.snapshot) {
                        match native
                            .host()
                            .save_settings(preferences.clone(), snapshot.preferences_revision)
                        {
                            Ok(_) => self.status = "设置已保存".to_string(),
                            Err(error) => self.status = error.to_string(),
                        }
                    }
                }
            }
        }

        fn history_ui(&mut self, ui: &mut egui::Ui) {
            ui.heading("历史");
            let Some(backend) = self.backend() else {
                return;
            };
            match backend.list_history() {
                Ok(history) if history.is_empty() => {
                    ui.label("暂无历史记录");
                }
                Ok(history) => {
                    for item in history.into_iter().rev().take(20) {
                        let delivery = match item.insert_status {
                            HistoryInsertStatus::Inserted => "已插入",
                            HistoryInsertStatus::CopiedFallback => "已复制",
                            HistoryInsertStatus::PasteSent => "已发送粘贴",
                            HistoryInsertStatus::Failed => "失败",
                            HistoryInsertStatus::NotRequested => "未请求插入",
                        };
                        ui.label(format!(
                            "{} · {} · {}",
                            item.created_at, delivery, item.final_text
                        ));
                    }
                }
                Err(error) => {
                    ui.label(error.to_string());
                }
            }
        }
    }

    impl eframe::App for OpenLessEguiApp {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            self.poll(ctx);
            egui::TopBottomPanel::top("status").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("OpenLess 2.0");
                    ui.separator();
                    ui.label(&self.status);
                });
            });
            egui::CentralPanel::default().show(ctx, |ui| {
                if let Some(error) = &self.startup_error {
                    ui.heading("启动失败");
                    ui.colored_label(egui::Color32::RED, error);
                    return;
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.dictation_ui(ui);
                    ui.separator();
                    self.less_computer_ui(ui);
                    ui.separator();
                    self.models_ui(ui);
                    ui.separator();
                    self.settings_ui(ui);
                    ui.separator();
                    self.history_ui(ui);
                });
            });
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    impl Drop for OpenLessEguiApp {
        fn drop(&mut self) {
            if let Some(native) = self.native.take() {
                let _ = self.tokio.block_on(native.shutdown());
            }
        }
    }

    fn package_kind() -> LinuxPackageKind {
        if std::env::var_os("APPDIR").is_some() {
            LinuxPackageKind::AppImage
        } else if cfg!(debug_assertions) {
            LinuxPackageKind::Development
        } else {
            LinuxPackageKind::SystemPackage
        }
    }

    fn backend_config() -> Result<BackendConfig, String> {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let data_dir = std::env::var_os("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".local/share")))
            .ok_or_else(|| "HOME/XDG_DATA_HOME is unavailable".to_string())?
            .join("OpenLess");
        let cache_dir = std::env::var_os("XDG_CACHE_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".cache")))
            .ok_or_else(|| "HOME/XDG_CACHE_HOME is unavailable".to_string())?
            .join("OpenLess");
        std::fs::create_dir_all(&data_dir).map_err(|error| error.to_string())?;
        std::fs::create_dir_all(&cache_dir).map_err(|error| error.to_string())?;
        let kind = package_kind();
        let capabilities = LinuxCapabilitySnapshot::detect(false, kind).capabilities;
        Ok(BackendConfig {
            data_dir,
            cache_dir,
            home_dir: home,
            resource_dir: std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(std::path::Path::to_path_buf)),
            platform: capabilities,
            locale: std::env::var("LANG").unwrap_or_else(|_| "en-US".to_string()),
        })
    }

    pub fn run() -> Result<(), String> {
        let tokio = Arc::new(tokio::runtime::Runtime::new().map_err(|error| error.to_string())?);
        let config = backend_config()?;
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| config.cache_dir.join("runtime"));
        let args = std::env::args().collect::<Vec<_>>();
        let broker = match SingleInstanceBroker::acquire_or_forward(
            &runtime_dir.join("openless.lock"),
            &runtime_dir.join("openless.sock"),
            LinuxLaunchIntent::from_args(&args),
        )
        .map_err(|error| error.to_string())?
        {
            SingleInstanceRole::Primary(broker) => broker,
            SingleInstanceRole::Forwarded => return Ok(()),
        };
        let native = (|| {
            let hotkeys = Fcitx5HotkeyListener::start().map_err(|error| error.to_string())?;
            let backend = LinuxBackendBuilder::from_shared_providers(config)
                .map_err(|error| error.to_string())?
                .build()
                .map_err(|error| error.to_string())?;
            tokio
                .block_on(LinuxNativeRuntime::start(
                    backend,
                    Some(broker),
                    Some(hotkeys),
                ))
                .map_err(|error| error.to_string())
        })();
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([960.0, 720.0]),
            ..Default::default()
        };
        eframe::run_native(
            "OpenLess",
            options,
            Box::new(move |_| Ok(Box::new(OpenLessEguiApp::new(tokio, native)))),
        )
        .map_err(|error| error.to_string())
    }
}

#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = linux_app::run() {
        eprintln!("OpenLess Linux UI failed: {error}");
        std::process::exit(1);
    }
}
