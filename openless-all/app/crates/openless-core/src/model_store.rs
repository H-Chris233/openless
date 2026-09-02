//! 跨平台模型清单、下载和缓存状态。
//!
//! 该模块只接收宿主已经解析好的目录；网络、文件系统和进度事件均通过窄
//! Adapter 注入，因此 Tauri/Linux 不需要再维护一套 Range/校验实现。

use std::collections::BTreeSet;
use std::io::{Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::errors::{BackendError, BackendErrorCode};
use crate::local_asr_catalog::{LocalAsrRuntime, LocalAsrTarget};

pub const MODEL_READY_SENTINEL: &str = ".openless-model-ready";
pub const MODEL_PARTIAL_INDEX: &str = ".partial.idx";
pub const DEFAULT_MODEL_CHUNK_BYTES: u64 = 32 * 1024 * 1024;
pub const DEFAULT_MODEL_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const DEFAULT_MODEL_MAX_RETRIES: u8 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTransportRequest {
    pub url: String,
    /// Inclusive byte range. `None` requests the complete object.
    pub range: Option<(u64, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTransportResponse {
    pub status: u16,
    pub bytes: Vec<u8>,
    pub total_bytes: Option<u64>,
}

pub trait ModelTransport: Send + Sync {
    fn request(
        &self,
        request: ModelTransportRequest,
    ) -> BoxFuture<'static, Result<ModelTransportResponse, BackendError>>;
}

#[derive(Clone)]
pub struct ReqwestModelTransport {
    client: reqwest::Client,
}

impl ReqwestModelTransport {
    pub fn new() -> Result<Self, BackendError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|error| BackendError::new(BackendErrorCode::Internal, error.to_string()))?;
        Ok(Self { client })
    }
}

impl ModelTransport for ReqwestModelTransport {
    fn request(
        &self,
        request: ModelTransportRequest,
    ) -> BoxFuture<'static, Result<ModelTransportResponse, BackendError>> {
        let client = self.client.clone();
        Box::pin(async move {
            let mut builder = client.get(&request.url);
            if let Some((start, end)) = request.range {
                builder = builder.header(reqwest::header::RANGE, format!("bytes={start}-{end}"));
            }
            let response = builder.send().await.map_err(|error| {
                BackendError::new(BackendErrorCode::Provider, error.to_string()).retryable(true)
            })?;
            let status = response.status().as_u16();
            let total_bytes = response.content_length().or_else(|| {
                response
                    .headers()
                    .get("content-range")
                    .and_then(|value| value.to_str().ok()?.rsplit('/').next()?.parse().ok())
            });
            let bytes = response.bytes().await.map_err(|error| {
                BackendError::new(BackendErrorCode::Provider, error.to_string()).retryable(true)
            })?;
            Ok(ModelTransportResponse {
                status,
                bytes: bytes.to_vec(),
                total_bytes,
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelFile {
    pub path: String,
    pub url: String,
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelManifest {
    pub model_id: String,
    pub repository: String,
    pub files: Vec<ModelFile>,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalogEntry {
    pub target: LocalAsrTarget,
    pub repository: String,
    pub display_name: String,
}

#[derive(Debug, Clone)]
pub struct ModelCatalog {
    entries: Vec<ModelCatalogEntry>,
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::standard()
    }
}

impl ModelCatalog {
    pub fn standard() -> Self {
        let mut entries = Vec::new();
        let add =
            |entries: &mut Vec<ModelCatalogEntry>, runtime, ids: &[&str], repository: &str| {
                for id in ids {
                    if let Ok(target) = LocalAsrTarget::parse(runtime, *id) {
                        entries.push(ModelCatalogEntry {
                            target,
                            repository: repository.into(),
                            display_name: (*id).into(),
                        });
                    }
                }
            };
        add(
            &mut entries,
            LocalAsrRuntime::Generic,
            &["qwen3-asr-0.6b"],
            "Qwen/Qwen3-ASR-0.6B",
        );
        add(
            &mut entries,
            LocalAsrRuntime::Generic,
            &["qwen3-asr-1.7b"],
            "Qwen/Qwen3-ASR-1.7B",
        );
        add(
            &mut entries,
            LocalAsrRuntime::Generic,
            &[
                "whisper-base",
                "whisper-small",
                "whisper-medium",
                "whisper-large-v3",
                "whisper-large-v3-turbo",
                "whisper-large-v3-turbo-q5",
            ],
            "ggerganov/whisper.cpp",
        );
        add(
            &mut entries,
            LocalAsrRuntime::Foundry,
            &[
                "whisper-small",
                "whisper-medium",
                "whisper-large-v3-turbo",
                "whisper-base",
                "whisper-tiny",
            ],
            "microsoft/whisper",
        );
        add(
            &mut entries,
            LocalAsrRuntime::SherpaOnnx,
            &[
                "sense-voice-small-zh",
                "paraformer-zh",
                "whisper-small-multi",
                "whisper-large-v3-multi",
                "qwen3-asr-0.6b-int8",
                "zipformer-bilingual-zh-en-streaming",
            ],
            "k2-fsa/sherpa-onnx",
        );
        Self { entries }
    }

    pub fn entries(&self) -> &[ModelCatalogEntry] {
        &self.entries
    }

    pub fn find(&self, runtime: LocalAsrRuntime, model_id: &str) -> Option<&ModelCatalogEntry> {
        self.entries
            .iter()
            .find(|entry| entry.target.runtime == runtime && entry.target.model_id() == model_id)
    }
}

impl Default for ModelCatalogEntry {
    fn default() -> Self {
        let target = LocalAsrTarget::parse(LocalAsrRuntime::Generic, "qwen3-asr-0.6b")
            .expect("built-in model id");
        Self {
            target,
            repository: "Qwen/Qwen3-ASR-0.6B".into(),
            display_name: "qwen3-asr-0.6b".into(),
        }
    }
}

impl ModelManifest {
    pub fn new(
        model_id: impl Into<String>,
        repository: impl Into<String>,
        files: Vec<ModelFile>,
    ) -> Result<Self, BackendError> {
        let model_id = model_id.into();
        let repository = repository.into();
        validate_model_id(&model_id)?;
        let mut seen = BTreeSet::new();
        for file in &files {
            validate_model_path(&file.path)?;
            validate_model_url(&file.url)?;
            if !seen.insert(file.path.clone()) {
                return Err(invalid("model manifest contains duplicate files"));
            }
            if file.size_bytes > DEFAULT_MODEL_MAX_FILE_BYTES {
                return Err(invalid("model file exceeds the configured size limit"));
            }
        }
        if files.is_empty() {
            return Err(invalid("model manifest must contain at least one file"));
        }
        let total_bytes = files.iter().map(|file| file.size_bytes).sum();
        Ok(Self {
            model_id,
            repository,
            files,
            total_bytes,
        })
    }

    pub fn from_hf_pages(
        model_id: impl Into<String>,
        repository: impl Into<String>,
        pages: &[Vec<serde_json::Value>],
    ) -> Result<Self, BackendError> {
        let model_id = model_id.into();
        let repository = repository.into();
        let files = merge_hf_tree_pages(&repository, &model_id, pages)?;
        Self::new(model_id, repository, files)
    }

    pub fn from_hf_pages_with_base(
        model_id: impl Into<String>,
        repository: impl Into<String>,
        pages: &[Vec<serde_json::Value>],
        base_url: &str,
    ) -> Result<Self, BackendError> {
        let model_id = model_id.into();
        let repository = repository.into();
        let files = merge_hf_tree_pages_with_base(&repository, &model_id, pages, base_url)?;
        Self::new(model_id, repository, files)
    }
}

#[derive(Debug, Clone)]
pub struct ModelStoreConfig {
    pub models_root_dir: PathBuf,
    pub chunk_size_bytes: u64,
    pub max_file_bytes: u64,
    pub max_retries: u8,
}

impl ModelStoreConfig {
    pub fn new(models_root_dir: PathBuf) -> Result<Self, BackendError> {
        if !models_root_dir.is_absolute() {
            return Err(invalid("model root directory must be absolute"));
        }
        if models_root_dir
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(invalid("model root directory cannot contain '..'"));
        }
        Ok(Self {
            models_root_dir,
            chunk_size_bytes: DEFAULT_MODEL_CHUNK_BYTES,
            max_file_bytes: DEFAULT_MODEL_MAX_FILE_BYTES,
            max_retries: DEFAULT_MODEL_MAX_RETRIES,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelDownloadPhase {
    Started,
    Progress,
    Finished,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDownloadProgress {
    pub model_id: String,
    pub file: String,
    pub file_index: usize,
    pub file_count: usize,
    pub bytes_downloaded: u64,
    pub bytes_total: u64,
    pub phase: ModelDownloadPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub trait DownloadProgressSink: Send + Sync {
    fn publish(&self, progress: ModelDownloadProgress);
}

impl<F> DownloadProgressSink for F
where
    F: Fn(ModelDownloadProgress) + Send + Sync,
{
    fn publish(&self, progress: ModelDownloadProgress) {
        self(progress)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCacheStatus {
    pub model_id: String,
    pub ready: bool,
    pub downloaded_bytes: u64,
    pub expected_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCard {
    pub model_id: String,
    pub repository: String,
    pub downloads: u64,
    pub likes: u64,
    pub description: String,
}

pub struct ModelStore {
    config: ModelStoreConfig,
    catalog: ModelCatalog,
    transport: Arc<dyn ModelTransport>,
    progress: Option<Arc<dyn DownloadProgressSink>>,
    progress_clock: Arc<Mutex<u64>>,
    // ponytail: one process-wide set serializes same-model downloads; per-model locks if parallelism matters.
    active_downloads: Arc<Mutex<BTreeSet<String>>>,
}

impl ModelStore {
    pub fn new(config: ModelStoreConfig) -> Result<Self, BackendError> {
        Ok(Self::with_transport(
            config,
            Arc::new(ReqwestModelTransport::new()?),
        ))
    }

    pub fn with_transport(config: ModelStoreConfig, transport: Arc<dyn ModelTransport>) -> Self {
        Self {
            config,
            catalog: ModelCatalog::standard(),
            transport,
            progress: None,
            progress_clock: Arc::new(Mutex::new(0)),
            active_downloads: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    pub fn with_progress_sink(mut self, sink: Arc<dyn DownloadProgressSink>) -> Self {
        self.progress = Some(sink);
        self
    }

    pub fn config(&self) -> &ModelStoreConfig {
        &self.config
    }

    pub fn catalog(&self) -> &ModelCatalog {
        &self.catalog
    }

    pub async fn fetch_hf_manifest(
        &self,
        model_id: &str,
        repository: &str,
        base_url: &str,
    ) -> Result<ModelManifest, BackendError> {
        validate_model_id(model_id)?;
        let base_url = base_url.trim_end_matches('/');
        validate_model_url(&format!("{base_url}/"))?;
        let mut pages = Vec::new();
        let mut cursor = None::<String>;
        let mut seen_cursors = BTreeSet::new();
        for _ in 0..100 {
            let mut url = format!("{base_url}/api/models/{repository}/tree/main?limit=1000");
            if let Some(cursor) = &cursor {
                url.push_str("&cursor=");
                url.push_str(cursor);
            }
            let response = self
                .transport
                .request(ModelTransportRequest { url, range: None })
                .await?;
            if response.status != 200 {
                return Err(BackendError::new(
                    BackendErrorCode::Provider,
                    format!("model manifest request returned HTTP {}", response.status),
                ));
            }
            let value: serde_json::Value = serde_json::from_slice(&response.bytes)
                .map_err(|error| invalid(format!("invalid model manifest JSON: {error}")))?;
            let entries = value
                .as_array()
                .cloned()
                .or_else(|| {
                    value
                        .get("entries")
                        .and_then(|items| items.as_array())
                        .cloned()
                })
                .ok_or_else(|| invalid("model manifest response must be an array"))?;
            pages.push(entries);
            let next = value
                .get("nextCursor")
                .or_else(|| value.get("next_cursor"))
                .and_then(|value| value.as_str())
                .map(str::to_string);
            let Some(next) = next else {
                break;
            };
            if !seen_cursors.insert(next.clone()) {
                return Err(invalid("model manifest cursor repeated"));
            }
            cursor = Some(next);
        }
        if cursor.is_some() && seen_cursors.len() >= 100 {
            return Err(invalid("model manifest pagination exceeded the page limit"));
        }
        ModelManifest::from_hf_pages_with_base(model_id, repository, &pages, base_url)
    }

    pub async fn fetch_hf_model_card(
        &self,
        model_id: &str,
        repository: &str,
        base_url: &str,
    ) -> Result<ModelCard, BackendError> {
        validate_model_id(model_id)?;
        let base_url = base_url.trim_end_matches('/');
        validate_model_url(&format!("{base_url}/"))?;
        let response = self
            .transport
            .request(ModelTransportRequest {
                url: format!("{base_url}/api/models/{repository}"),
                range: None,
            })
            .await?;
        if response.status != 200 {
            return Err(BackendError::new(
                BackendErrorCode::Provider,
                format!("model card request returned HTTP {}", response.status),
            ));
        }
        let value: serde_json::Value = serde_json::from_slice(&response.bytes)
            .map_err(|error| invalid(format!("invalid model card JSON: {error}")))?;
        let description = value
            .pointer("/cardData/summary")
            .or_else(|| value.get("description"))
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .chars()
            .take(280)
            .collect();
        Ok(ModelCard {
            model_id: model_id.into(),
            repository: repository.into(),
            downloads: value
                .get("downloads")
                .and_then(|value| value.as_u64())
                .unwrap_or(0),
            likes: value
                .get("likes")
                .and_then(|value| value.as_u64())
                .unwrap_or(0),
            description,
        })
    }

    pub fn model_dir(&self, model_id: &str) -> Result<PathBuf, BackendError> {
        validate_model_id(model_id)?;
        Ok(self.config.models_root_dir.join(model_id))
    }

    pub fn status(&self, manifest: &ModelManifest) -> Result<ModelCacheStatus, BackendError> {
        let dir = self.model_dir(&manifest.model_id)?;
        let downloaded_bytes = manifest
            .files
            .iter()
            .map(|file| {
                std::fs::metadata(dir.join(&file.path))
                    .map(|meta| meta.len())
                    .unwrap_or(0)
            })
            .fold(0u64, u64::saturating_add);
        let complete_files = manifest.files.iter().all(|file| {
            std::fs::metadata(dir.join(&file.path))
                .map(|meta| meta.len() == file.size_bytes)
                .unwrap_or(false)
        });
        Ok(ModelCacheStatus {
            model_id: manifest.model_id.clone(),
            ready: dir.join(MODEL_READY_SENTINEL).is_file() && complete_files,
            downloaded_bytes,
            expected_bytes: manifest.total_bytes,
        })
    }

    pub async fn download(
        &self,
        manifest: ModelManifest,
        cancelled: Arc<AtomicBool>,
    ) -> Result<ModelCacheStatus, BackendError> {
        self.download_with_manifest(manifest, cancelled).await
    }

    async fn download_with_manifest(
        &self,
        manifest: ModelManifest,
        cancelled: Arc<AtomicBool>,
    ) -> Result<ModelCacheStatus, BackendError> {
        let already_active = {
            let mut active = self
                .active_downloads
                .lock()
                .expect("model download lock poisoned");
            !active.insert(manifest.model_id.clone())
        };
        if already_active {
            return Err(BackendError::new(
                BackendErrorCode::Busy,
                "model download is already in progress",
            ));
        }
        let _active_guard = ActiveDownloadGuard {
            active: Arc::clone(&self.active_downloads),
            model_id: manifest.model_id.clone(),
        };
        if manifest
            .files
            .iter()
            .any(|file| file.size_bytes > self.config.max_file_bytes)
        {
            return Err(invalid("model file exceeds the configured size limit"));
        }
        std::fs::create_dir_all(&self.config.models_root_dir).map_err(platform_error)?;
        let staging = self
            .config
            .models_root_dir
            .join(format!(".{}.staging", manifest.model_id));
        std::fs::create_dir_all(&staging).map_err(platform_error)?;
        self.emit(&manifest, "", 0, ModelDownloadPhase::Started, None);
        let mut downloaded_before: u64 = manifest
            .files
            .iter()
            .map(|file| {
                std::fs::metadata(staging.join(&file.path))
                    .map(|meta| meta.len().min(file.size_bytes))
                    .unwrap_or(0)
            })
            .sum();
        for (file_index, file) in manifest.files.iter().enumerate() {
            if cancelled.load(Ordering::Acquire) {
                self.emit(
                    &manifest,
                    &file.path,
                    file_index,
                    ModelDownloadPhase::Cancelled,
                    None,
                );
                return Err(cancelled_error());
            }
            let path = staging.join(&file.path);
            validate_model_path(&file.path)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(platform_error)?;
            }
            let mut offset = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
            if offset > file.size_bytes {
                offset = 0;
                let _ = std::fs::remove_file(&path);
            }
            let mut output = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(offset == 0)
                .open(&path)
                .map_err(platform_error)?;
            if offset > 0 {
                output
                    .seek(std::io::SeekFrom::Start(offset))
                    .map_err(platform_error)?;
            }
            while offset < file.size_bytes {
                if cancelled.load(Ordering::Acquire) {
                    self.emit(
                        &manifest,
                        &file.path,
                        file_index,
                        ModelDownloadPhase::Cancelled,
                        None,
                    );
                    return Err(cancelled_error());
                }
                let end = (offset + self.config.chunk_size_bytes.max(1)).min(file.size_bytes) - 1;
                let mut response = None;
                let mut last_error = None;
                for attempt in 0..=self.config.max_retries {
                    match self
                        .transport
                        .request(ModelTransportRequest {
                            url: file.url.clone(),
                            range: Some((offset, end)),
                        })
                        .await
                    {
                        Ok(value)
                            if (offset == 0 && value.status == 200) || value.status == 206 =>
                        {
                            response = Some(value);
                            break;
                        }
                        Ok(value) => {
                            last_error =
                                Some(format!("unexpected model HTTP status {}", value.status))
                        }
                        Err(error) => last_error = Some(error.message),
                    }
                    if attempt < self.config.max_retries {
                        tokio::time::sleep(Duration::from_millis(
                            50u64.saturating_mul(1u64 << attempt.min(6)),
                        ))
                        .await;
                    }
                }
                let response = match response {
                    Some(response) => response,
                    None => {
                        let error = BackendError::new(
                            BackendErrorCode::Provider,
                            last_error.unwrap_or_else(|| "model download failed".into()),
                        )
                        .retryable(true);
                        self.emit(
                            &manifest,
                            &file.path,
                            file_index,
                            ModelDownloadPhase::Failed,
                            None,
                        );
                        return Err(error);
                    }
                };
                if response.bytes.len() as u64 > end - offset + 1 {
                    return Err(invalid("model server returned more bytes than requested"));
                }
                output.write_all(&response.bytes).map_err(platform_error)?;
                let received = response.bytes.len() as u64;
                if received == 0 {
                    return Err(BackendError::new(
                        BackendErrorCode::Provider,
                        "model server returned an empty range",
                    )
                    .retryable(true));
                }
                offset = offset.saturating_add(received);
                downloaded_before = downloaded_before.saturating_add(received);
                append_partial_index(&staging, &format!("{}\t{}", file.path, offset))?;
                self.emit(
                    &manifest,
                    &file.path,
                    file_index,
                    ModelDownloadPhase::Progress,
                    Some((downloaded_before, manifest.total_bytes)),
                );
            }
            output.flush().map_err(platform_error)?;
            if let Some(expected) = &file.sha256 {
                let actual = sha256_file(&path)?;
                if !actual.eq_ignore_ascii_case(expected) {
                    let _ = std::fs::remove_file(&path);
                    self.emit(
                        &manifest,
                        &file.path,
                        file_index,
                        ModelDownloadPhase::Failed,
                        None,
                    );
                    return Err(BackendError::new(
                        BackendErrorCode::Provider,
                        format!("checksum mismatch for {}", file.path),
                    ));
                }
            }
            append_partial_index(&staging, &file.path)?;
        }
        let sentinel = staging.join(MODEL_READY_SENTINEL);
        std::fs::write(&sentinel, b"ready\n").map_err(platform_error)?;
        let _ = std::fs::remove_file(staging.join(MODEL_PARTIAL_INDEX));
        let destination = self.model_dir(&manifest.model_id)?;
        commit_staging(&staging, &destination)?;
        self.emit(
            &manifest,
            "",
            manifest.files.len(),
            ModelDownloadPhase::Finished,
            Some((manifest.total_bytes, manifest.total_bytes)),
        );
        self.status(&manifest)
    }

    fn emit(
        &self,
        manifest: &ModelManifest,
        file: &str,
        file_index: usize,
        phase: ModelDownloadPhase,
        bytes: Option<(u64, u64)>,
    ) {
        let Some(sink) = &self.progress else {
            return;
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_millis() as u64)
            .unwrap_or(0);
        let mut last = self
            .progress_clock
            .lock()
            .expect("model progress lock poisoned");
        if phase == ModelDownloadPhase::Progress && now.saturating_sub(*last) < 150 {
            return;
        }
        *last = now;
        let (downloaded, total) = bytes.unwrap_or((0, manifest.total_bytes));
        sink.publish(ModelDownloadProgress {
            model_id: manifest.model_id.clone(),
            file: file.into(),
            file_index,
            file_count: manifest.files.len(),
            bytes_downloaded: downloaded,
            bytes_total: total,
            phase,
            error: None,
        });
    }

    pub fn cleanup_incomplete(&self, model_id: &str) -> Result<(), BackendError> {
        validate_model_id(model_id)?;
        let staging = self
            .config
            .models_root_dir
            .join(format!(".{}.staging", model_id));
        if staging.exists() {
            std::fs::remove_dir_all(staging).map_err(platform_error)?;
        }
        Ok(())
    }

    pub fn migrate_legacy_root(&self, legacy_root: &Path) -> Result<(), BackendError> {
        if !legacy_root.is_absolute() {
            return Err(invalid("legacy model root must be absolute"));
        }
        if !legacy_root.is_dir() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.config.models_root_dir).map_err(platform_error)?;
        for entry in std::fs::read_dir(legacy_root).map_err(platform_error)? {
            let entry = entry.map_err(platform_error)?;
            let source = entry.path();
            let name = entry.file_name();
            let destination = self.config.models_root_dir.join(&name);
            if destination.exists() {
                continue;
            }
            std::fs::rename(&source, &destination)
                .or_else(|_| copy_dir_recursive(&source, &destination))
                .map_err(platform_error)?;
            migrate_ready_sentinel(&destination)?;
        }
        Ok(())
    }
}

fn migrate_ready_sentinel(model_dir: &Path) -> Result<(), BackendError> {
    if !model_dir.is_dir() || model_dir.join(MODEL_READY_SENTINEL).is_file() {
        return Ok(());
    }
    for legacy in [".openless-asr-ready", ".ready", "ready"] {
        let source = model_dir.join(legacy);
        if source.is_file() {
            std::fs::rename(source, model_dir.join(MODEL_READY_SENTINEL))
                .map_err(platform_error)?;
            break;
        }
    }
    Ok(())
}

fn append_partial_index(staging: &Path, file: &str) -> Result<(), BackendError> {
    let index = staging.join(MODEL_PARTIAL_INDEX);
    let mut handle = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(index)
        .map_err(platform_error)?;
    writeln!(handle, "{file}").map_err(platform_error)?;
    handle.sync_data().map_err(platform_error)
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> std::io::Result<()> {
    if source.is_dir() {
        std::fs::create_dir_all(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_dir_recursive(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(source, destination)?;
    }
    Ok(())
}

fn commit_staging(staging: &Path, destination: &Path) -> Result<(), BackendError> {
    let backup = destination.with_file_name(format!(
        ".{}.previous-{}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("model"),
        uuid::Uuid::new_v4().simple()
    ));
    let had_previous = destination.exists();
    if had_previous {
        std::fs::rename(destination, &backup).map_err(platform_error)?;
    }
    if let Err(error) = std::fs::rename(staging, destination) {
        if had_previous {
            let _ = std::fs::rename(&backup, destination);
        }
        return Err(platform_error(error));
    }
    if had_previous {
        std::fs::remove_dir_all(backup).map_err(platform_error)?;
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, BackendError> {
    let mut file = std::fs::File::open(path).map_err(platform_error)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(platform_error)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn validate_model_id(value: &str) -> Result<(), BackendError> {
    if value.is_empty()
        || value.len() > 128
        || value.contains('/')
        || value.contains('\\')
        || value.contains('\0')
        || value == "."
        || value == ".."
    {
        return Err(invalid("invalid model id"));
    }
    Ok(())
}

pub fn validate_model_path(value: &str) -> Result<(), BackendError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || value.contains('\\')
        || value.contains('\0')
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(invalid("model manifest contains an unsafe path"));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct HfTreeEntry {
    #[serde(rename = "type")]
    entry_type: String,
    path: String,
    #[serde(default)]
    size: Option<u64>,
}

pub fn parse_hf_tree_page(
    repository: &str,
    model_id: &str,
    entries: &[serde_json::Value],
) -> Result<Vec<ModelFile>, BackendError> {
    validate_model_id(model_id)?;
    validate_repository(repository)?;
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    for value in entries {
        let entry: HfTreeEntry = serde_json::from_value(value.clone())
            .map_err(|_| invalid("invalid Hugging Face tree entry"))?;
        if entry.entry_type != "file" {
            continue;
        }
        validate_model_path(&entry.path)?;
        if !keep_model_file(&entry.path) {
            continue;
        }
        if !seen.insert(entry.path.clone()) {
            return Err(invalid("duplicate file in model tree"));
        }
        let size_bytes = entry.size.unwrap_or(0);
        if size_bytes > DEFAULT_MODEL_MAX_FILE_BYTES {
            return Err(invalid("model file exceeds the configured size limit"));
        }
        files.push(ModelFile {
            url: format!(
                "https://huggingface.co/{repository}/resolve/main/{}",
                entry.path
            ),
            path: entry.path,
            size_bytes,
            sha256: None,
        });
    }
    if files.is_empty() {
        return Err(invalid("Hugging Face tree returned no model files"));
    }
    Ok(files)
}

/// Merge paginated Hugging Face tree responses while rejecting duplicate paths
/// across page boundaries.
pub fn merge_hf_tree_pages(
    repository: &str,
    model_id: &str,
    pages: &[Vec<serde_json::Value>],
) -> Result<Vec<ModelFile>, BackendError> {
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    for page in pages {
        for file in parse_hf_tree_page(repository, model_id, page)? {
            if !seen.insert(file.path.clone()) {
                return Err(invalid("duplicate file across Hugging Face tree pages"));
            }
            files.push(file);
        }
    }
    if files.is_empty() {
        return Err(invalid("Hugging Face tree returned no model files"));
    }
    Ok(files)
}

pub fn merge_hf_tree_pages_with_base(
    repository: &str,
    model_id: &str,
    pages: &[Vec<serde_json::Value>],
    base_url: &str,
) -> Result<Vec<ModelFile>, BackendError> {
    validate_model_id(model_id)?;
    validate_repository(repository)?;
    let base_url = base_url.trim_end_matches('/');
    validate_model_url(&format!("{base_url}/"))?;
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    for page in pages {
        for value in page {
            let entry: HfTreeEntry = serde_json::from_value(value.clone())
                .map_err(|_| invalid("invalid Hugging Face tree entry"))?;
            if entry.entry_type != "file" {
                continue;
            }
            validate_model_path(&entry.path)?;
            if !keep_model_file(&entry.path) {
                continue;
            }
            if !seen.insert(entry.path.clone()) {
                return Err(invalid("duplicate file across Hugging Face tree pages"));
            }
            files.push(ModelFile {
                url: format!("{base_url}/{repository}/resolve/main/{}", entry.path),
                path: entry.path,
                size_bytes: entry.size.unwrap_or(0),
                sha256: None,
            });
        }
    }
    if files.is_empty() {
        return Err(invalid("Hugging Face tree returned no model files"));
    }
    Ok(files)
}

fn keep_model_file(path: &str) -> bool {
    if path.starts_with('.') {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    if [".md", ".png", ".jpg", ".jpeg", ".gif", ".svg"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
    {
        return false;
    }
    matches!(
        lower.rsplit('.').next().unwrap_or(""),
        "json"
            | "safetensors"
            | "txt"
            | "tokens"
            | "vocab"
            | "bin"
            | "model"
            | "tiktoken"
            | "onnx"
            | "yaml"
            | "yml"
    )
}

fn validate_repository(repository: &str) -> Result<(), BackendError> {
    if repository.trim().is_empty()
        || repository.contains('\\')
        || repository.contains("..")
        || repository.starts_with('/')
        || repository.chars().any(|character| {
            !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | '/'))
        })
    {
        return Err(invalid("model repository is invalid"));
    }
    Ok(())
}

pub fn extract_archive_safely(
    bytes: &[u8],
    destination: &Path,
    max_file_bytes: u64,
) -> Result<(), BackendError> {
    if !destination.is_absolute() {
        return Err(invalid("archive destination must be absolute"));
    }
    let staging = destination.with_file_name(format!(
        ".{}.archive-staging-{}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("model"),
        uuid::Uuid::new_v4().simple()
    ));
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(platform_error)?;
    }
    std::fs::create_dir_all(&staging).map_err(platform_error)?;
    let mut staging_guard = ArchiveStagingGuard {
        path: staging.clone(),
        committed: false,
    };
    let reader = std::io::Cursor::new(bytes);
    let mut archive = match zip::ZipArchive::new(reader) {
        Ok(archive) => archive,
        Err(error) => {
            return Err(invalid(error.to_string()));
        }
    };
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| invalid(error.to_string()))?;
        if let Err(error) = validate_model_path(entry.name()) {
            return Err(error);
        }
        if entry.is_dir() {
            continue;
        }
        if entry.size() > max_file_bytes {
            return Err(invalid("archive entry exceeds the configured size limit"));
        }
        let output = staging.join(entry.name());
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).map_err(platform_error)?;
        }
        let mut file = std::fs::File::create(output).map_err(platform_error)?;
        std::io::copy(&mut entry, &mut file).map_err(platform_error)?;
    }
    commit_staging(&staging, destination)?;
    staging_guard.committed = true;
    Ok(())
}

struct ArchiveStagingGuard {
    path: PathBuf,
    committed: bool,
}

struct ActiveDownloadGuard {
    active: Arc<Mutex<BTreeSet<String>>>,
    model_id: String,
}

impl Drop for ActiveDownloadGuard {
    fn drop(&mut self) {
        self.active
            .lock()
            .expect("model download lock poisoned")
            .remove(&self.model_id);
    }
}

impl Drop for ArchiveStagingGuard {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn invalid(message: impl Into<String>) -> BackendError {
    BackendError::new(BackendErrorCode::InvalidArgument, message)
}

pub fn validate_model_url(value: &str) -> Result<(), BackendError> {
    let parsed = url::Url::parse(value).map_err(|_| invalid("model file URL is invalid"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(invalid("model file URL must use http or https"));
    }
    Ok(())
}
fn platform_error(error: impl std::fmt::Display) -> BackendError {
    BackendError::new(BackendErrorCode::Platform, error.to_string())
}
fn cancelled_error() -> BackendError {
    BackendError::new(BackendErrorCode::Cancelled, "model operation cancelled")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct FakeTransport {
        calls: Arc<AtomicUsize>,
        body: Vec<u8>,
    }
    impl ModelTransport for FakeTransport {
        fn request(
            &self,
            request: ModelTransportRequest,
        ) -> BoxFuture<'static, Result<ModelTransportResponse, BackendError>> {
            let calls = Arc::clone(&self.calls);
            let body = self.body.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::Relaxed);
                let (status, bytes) = match request.range {
                    Some((start, end)) => (
                        206,
                        body[start as usize..=(end as usize).min(body.len() - 1)].to_vec(),
                    ),
                    None => (200, body),
                };
                Ok(ModelTransportResponse {
                    status,
                    total_bytes: Some(bytes.len() as u64),
                    bytes,
                })
            })
        }
    }

    #[test]
    fn path_validation_rejects_traversal_and_absolute_names() {
        assert!(validate_model_path("weights/model.bin").is_ok());
        assert!(validate_model_path("../model.bin").is_err());
        assert!(validate_model_path("/tmp/model.bin").is_err());
        assert!(validate_model_path("C:\\\\model.bin").is_err());
    }

    #[test]
    fn hf_tree_filters_directories_and_rejects_duplicates() {
        let entries = vec![
            serde_json::json!({"type":"directory","path":"nested"}),
            serde_json::json!({"type":"file","path":"weights.bin","size":3}),
        ];
        let files = parse_hf_tree_page("Qwen/demo", "qwen-demo", &entries).unwrap();
        assert_eq!(files[0].path, "weights.bin");
        assert!(parse_hf_tree_page(
            "Qwen/demo",
            "qwen-demo",
            &[serde_json::json!({"type":"file","path":"../x"})]
        )
        .is_err());
    }

    #[tokio::test]
    async fn download_resumes_ranges_and_writes_ready_sentinel() {
        let root =
            std::env::temp_dir().join(format!("openless-model-store-{}", uuid::Uuid::new_v4()));
        let body = b"0123456789".to_vec();
        let calls = Arc::new(AtomicUsize::new(0));
        let transport = Arc::new(FakeTransport {
            calls: Arc::clone(&calls),
            body: body.clone(),
        });
        let mut config = ModelStoreConfig::new(root.clone()).unwrap();
        config.chunk_size_bytes = 4;
        let store = ModelStore::with_transport(config, transport);
        let manifest = ModelManifest::new(
            "demo",
            "org/demo",
            vec![ModelFile {
                path: "weights.bin".into(),
                url: "https://example.test/weights.bin".into(),
                size_bytes: body.len() as u64,
                sha256: Some(format!("{:x}", Sha256::digest(&body))),
            }],
        )
        .unwrap();
        let status = store
            .download(manifest.clone(), Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert!(status.ready);
        assert_eq!(std::fs::read(root.join("demo/weights.bin")).unwrap(), body);
        assert!(calls.load(Ordering::Relaxed) >= 3);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn archive_extraction_rejects_parent_paths() {
        let root =
            std::env::temp_dir().join(format!("openless-model-archive-{}", uuid::Uuid::new_v4()));
        assert!(extract_archive_safely(b"not-a-zip", &root, 1024).is_err());
    }
}
