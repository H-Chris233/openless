//! 跨平台模型清单、下载和缓存状态。
//!
//! 该模块拥有文件系统、Range/校验和进度状态；仅网络请求通过窄 Transport
//! 注入，因此 Tauri/Linux 不需要再维护第二套模型存储实现。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{future::BoxFuture, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domains::{LocalAsrModel, LocalAsrModelCard, LocalAsrRemoteFile, LocalAsrRemoteInfo};
use crate::errors::{BackendError, BackendErrorCode};
use crate::local_asr_catalog::{LocalAsrRuntime, LocalAsrTarget};

pub const MODEL_READY_SENTINEL: &str = ".openless-model-ready";
pub const MODEL_PARTIAL_INDEX: &str = ".partial.idx";
pub const DEFAULT_MODEL_CHUNK_BYTES: u64 = 32 * 1024 * 1024;
pub const DEFAULT_MODEL_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const DEFAULT_MODEL_MAX_TOTAL_BYTES: u64 = 32 * 1024 * 1024 * 1024;
pub const DEFAULT_MODEL_METADATA_BYTES: u64 = 8 * 1024 * 1024;
pub const DEFAULT_MODEL_MAX_RETRIES: u8 = 4;
const PARTIAL_INDEX_VERSION: u8 = 1;

pub fn model_mirror_base(
    mirror: crate::local_asr_catalog::LocalAsrMirror,
) -> Result<&'static str, BackendError> {
    match mirror {
        crate::local_asr_catalog::LocalAsrMirror::Huggingface => Ok("https://huggingface.co"),
        crate::local_asr_catalog::LocalAsrMirror::HfMirror => Ok("https://hf-mirror.com"),
        crate::local_asr_catalog::LocalAsrMirror::GithubRelease => Err(BackendError::new(
            BackendErrorCode::Unsupported,
            "GitHub release models use their catalog URL rather than a Hugging Face mirror",
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTransportRequest {
    pub url: String,
    /// Inclusive byte range. `None` requests the complete object.
    pub range: Option<(u64, u64)>,
    pub max_response_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelContentRange {
    pub start: u64,
    pub end: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelHttpMetadata {
    pub content_length: Option<u64>,
    pub content_range: Option<ModelContentRange>,
    pub link: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTransportResponse {
    pub status: u16,
    pub bytes: Vec<u8>,
    pub metadata: ModelHttpMetadata,
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
            let metadata = ModelHttpMetadata {
                content_length: response.content_length(),
                content_range: response
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_content_range),
                link: response
                    .headers()
                    .get(reqwest::header::LINK)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string),
            };
            if metadata
                .content_length
                .is_some_and(|length| length > request.max_response_bytes)
            {
                return Err(invalid("model response exceeds the configured size limit"));
            }
            let mut bytes = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| {
                    BackendError::new(BackendErrorCode::Provider, error.to_string()).retryable(true)
                })?;
                if bytes.len() as u64 + chunk.len() as u64 > request.max_response_bytes {
                    return Err(invalid("model response exceeds the configured size limit"));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(ModelTransportResponse {
                status,
                bytes,
                metadata,
            })
        })
    }
}

fn parse_content_range(value: &str) -> Option<ModelContentRange> {
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some(ModelContentRange {
        start: start.parse().ok()?,
        end: end.parse().ok()?,
        total: total.parse().ok()?,
    })
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<ModelArchiveSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelArchiveSpec {
    pub file_path: String,
    pub root_dir: String,
    pub required_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalogEntry {
    pub target: LocalAsrTarget,
    pub repository: String,
    pub display_name: String,
    pub family: String,
    pub mode: String,
    pub languages: Vec<String>,
    pub selector: ModelFileSelector,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFileMapping {
    pub remote_path: String,
    pub local_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelFileSelector {
    QwenRepository,
    Exact(Vec<ModelFileMapping>),
    Native,
    Archive {
        url: String,
        root_dir: String,
        size_bytes: u64,
        sha256: String,
        required_paths: Vec<String>,
    },
}

impl ModelFileSelector {
    fn local_path(&self, remote_path: &str) -> Option<String> {
        match self {
            Self::QwenRepository if qwen_model_file(remote_path) => Some(remote_path.to_string()),
            Self::Exact(files) => files
                .iter()
                .find(|file| file.remote_path == remote_path)
                .map(|file| file.local_path.clone()),
            Self::Native | Self::Archive { .. } | Self::QwenRepository => None,
        }
    }
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
        let mut add = |runtime,
                       id: &str,
                       repository: &str,
                       display_name: &str,
                       family: &str,
                       mode: &str,
                       languages: &[&str],
                       selector| {
            entries.push(ModelCatalogEntry {
                target: LocalAsrTarget::parse(runtime, id).expect("built-in model id"),
                repository: repository.into(),
                display_name: display_name.into(),
                family: family.into(),
                mode: mode.into(),
                languages: languages
                    .iter()
                    .map(|language| (*language).into())
                    .collect(),
                selector,
            });
        };
        for (id, repository) in [
            ("qwen3-asr-0.6b", "Qwen/Qwen3-ASR-0.6B"),
            ("qwen3-asr-1.7b", "Qwen/Qwen3-ASR-1.7B"),
        ] {
            add(
                LocalAsrRuntime::Generic,
                id,
                repository,
                id,
                "qwen3",
                "offline",
                &["multi"],
                ModelFileSelector::QwenRepository,
            );
        }
        for (id, file) in [
            ("whisper-base", "ggml-base.bin"),
            ("whisper-small", "ggml-small.bin"),
            ("whisper-medium", "ggml-medium.bin"),
            ("whisper-large-v3", "ggml-large-v3.bin"),
            ("whisper-large-v3-turbo", "ggml-large-v3-turbo.bin"),
            ("whisper-large-v3-turbo-q5", "ggml-large-v3-turbo-q5_0.bin"),
        ] {
            add(
                LocalAsrRuntime::Generic,
                id,
                "ggerganov/whisper.cpp",
                id,
                "whisper",
                "offline",
                &["multi"],
                exact(&[(file, file)]),
            );
        }
        for id in [
            "whisper-small",
            "whisper-medium",
            "whisper-large-v3-turbo",
            "whisper-base",
            "whisper-tiny",
        ] {
            add(
                LocalAsrRuntime::Foundry,
                id,
                "microsoft/whisper",
                id,
                "whisper",
                "offline",
                &["multi"],
                ModelFileSelector::Native,
            );
        }
        for (id, repository, display_name, family, mode, languages, files) in [
            (
                "sense-voice-small-zh",
                "csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17",
                "SenseVoice Small (zh/en/ja/ko/yue)",
                "sense_voice",
                "offline",
                &["zh", "en", "ja", "ko", "yue"][..].as_ref(),
                &[
                    ("model.int8.onnx", "model.int8.onnx"),
                    ("tokens.txt", "tokens.txt"),
                ][..],
            ),
            (
                "paraformer-zh",
                "csukuangfj/sherpa-onnx-paraformer-zh-2024-03-09",
                "Paraformer (zh)",
                "paraformer",
                "offline",
                &["zh"][..].as_ref(),
                &[
                    ("model.int8.onnx", "model.int8.onnx"),
                    ("tokens.txt", "tokens.txt"),
                ][..],
            ),
            (
                "whisper-small-multi",
                "csukuangfj/sherpa-onnx-whisper-small",
                "Whisper Small (multilingual)",
                "whisper",
                "offline",
                &["multi"][..].as_ref(),
                &[
                    ("small-encoder.int8.onnx", "encoder.int8.onnx"),
                    ("small-decoder.int8.onnx", "decoder.int8.onnx"),
                    ("small-tokens.txt", "tokens.txt"),
                ][..],
            ),
            (
                "whisper-large-v3-multi",
                "csukuangfj/sherpa-onnx-whisper-large-v3",
                "Whisper Large V3 (multilingual)",
                "whisper",
                "offline",
                &["multi"][..].as_ref(),
                &[
                    ("large-v3-encoder.int8.onnx", "encoder.int8.onnx"),
                    ("large-v3-decoder.int8.onnx", "decoder.int8.onnx"),
                    ("large-v3-tokens.txt", "tokens.txt"),
                ][..],
            ),
            (
                "zipformer-bilingual-zh-en-streaming",
                "csukuangfj/sherpa-onnx-streaming-zipformer-bilingual-zh-en-2023-02-20",
                "Zipformer Streaming bilingual (zh/en)",
                "zipformer",
                "online",
                &["zh", "en"][..].as_ref(),
                &[
                    (
                        "encoder-epoch-99-avg-1.int8.onnx",
                        "encoder-epoch-99-avg-1.int8.onnx",
                    ),
                    ("decoder-epoch-99-avg-1.onnx", "decoder-epoch-99-avg-1.onnx"),
                    (
                        "joiner-epoch-99-avg-1.int8.onnx",
                        "joiner-epoch-99-avg-1.int8.onnx",
                    ),
                    ("tokens.txt", "tokens.txt"),
                ][..],
            ),
        ] {
            add(
                LocalAsrRuntime::SherpaOnnx,
                id,
                repository,
                display_name,
                family,
                mode,
                languages,
                exact(files),
            );
        }
        add(
            LocalAsrRuntime::SherpaOnnx,
            "qwen3-asr-0.6b-int8",
            "",
            "Qwen3-ASR 0.6B INT8",
            "qwen3_asr",
            "offline",
            &["multi"],
            ModelFileSelector::Archive {
                url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2".into(),
                root_dir: "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25".into(),
                size_bytes: 878_702_423,
                sha256: "393f8a14e2f5fb96746aaab342997a40641001fbd5bf9592a080a8329178ee96".into(),
                required_paths: vec![
                    "conv_frontend.onnx".into(),
                    "encoder.int8.onnx".into(),
                    "decoder.int8.onnx".into(),
                    "tokenizer/tokenizer.json".into(),
                ],
            },
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
            family: "qwen3".into(),
            mode: "offline".into(),
            languages: vec!["multi".into()],
            selector: ModelFileSelector::QwenRepository,
        }
    }
}

fn exact(files: &[(&str, &str)]) -> ModelFileSelector {
    ModelFileSelector::Exact(
        files
            .iter()
            .map(|(remote_path, local_path)| ModelFileMapping {
                remote_path: (*remote_path).into(),
                local_path: (*local_path).into(),
            })
            .collect(),
    )
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
            if let Some(sha256) = &file.sha256 {
                if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(invalid("model file has an invalid sha256"));
                }
            }
        }
        if files.is_empty() {
            return Err(invalid("model manifest must contain at least one file"));
        }
        let total_bytes = files.iter().try_fold(0u64, |total, file| {
            total
                .checked_add(file.size_bytes)
                .ok_or_else(|| invalid("model total size overflowed"))
        })?;
        Ok(Self {
            model_id,
            repository,
            files,
            total_bytes,
            archive: None,
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
    pub max_total_bytes: u64,
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
            max_total_bytes: DEFAULT_MODEL_MAX_TOTAL_BYTES,
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
    pub runtime: LocalAsrRuntime,
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
    models_root_dir: Arc<std::sync::RwLock<PathBuf>>,
    catalog: ModelCatalog,
    transport: Arc<dyn ModelTransport>,
    progress: Arc<std::sync::RwLock<Option<Arc<dyn DownloadProgressSink>>>>,
    progress_clock: Arc<Mutex<HashMap<String, u64>>>,
    active_downloads: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

impl ModelStore {
    pub fn new(config: ModelStoreConfig) -> Result<Self, BackendError> {
        Ok(Self::with_transport(
            config,
            Arc::new(ReqwestModelTransport::new()?),
        ))
    }

    pub fn with_transport(config: ModelStoreConfig, transport: Arc<dyn ModelTransport>) -> Self {
        let models_root_dir = Arc::new(std::sync::RwLock::new(config.models_root_dir.clone()));
        Self {
            config,
            models_root_dir,
            catalog: ModelCatalog::standard(),
            transport,
            progress: Arc::new(std::sync::RwLock::new(None)),
            progress_clock: Arc::new(Mutex::new(HashMap::new())),
            active_downloads: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_progress_sink(self, sink: Arc<dyn DownloadProgressSink>) -> Self {
        *self
            .progress
            .write()
            .expect("model progress sink lock poisoned") = Some(sink);
        self
    }

    pub fn set_progress_sink(&self, sink: Arc<dyn DownloadProgressSink>) {
        *self
            .progress
            .write()
            .expect("model progress sink lock poisoned") = Some(sink);
    }

    pub fn config(&self) -> &ModelStoreConfig {
        &self.config
    }

    pub fn catalog(&self) -> &ModelCatalog {
        &self.catalog
    }

    pub fn models_root_dir(&self) -> PathBuf {
        self.models_root_dir
            .read()
            .expect("model root lock poisoned")
            .clone()
    }

    pub fn list_models(
        &self,
        runtime: LocalAsrRuntime,
    ) -> Result<Vec<LocalAsrModel>, BackendError> {
        self.catalog
            .entries()
            .iter()
            .filter(|entry| entry.target.runtime == runtime)
            .map(|entry| {
                let directory = self.model_dir(entry.target.model_id())?;
                Ok(LocalAsrModel {
                    target: entry.target.clone(),
                    display_name: entry.display_name.clone(),
                    family: entry.family.clone(),
                    mode: Some(entry.mode.clone()),
                    repository: (!entry.repository.is_empty()).then(|| entry.repository.clone()),
                    languages: entry.languages.clone(),
                    installed: directory.join(MODEL_READY_SENTINEL).is_file(),
                    downloaded_bytes: directory_size(&directory).unwrap_or(0),
                    size_bytes: None,
                })
            })
            .collect()
    }

    pub async fn remote_info(
        &self,
        target: LocalAsrTarget,
        mirror: crate::local_asr_catalog::LocalAsrMirror,
    ) -> Result<LocalAsrRemoteInfo, BackendError> {
        let entry = self
            .catalog
            .find(target.runtime, target.model_id())
            .ok_or_else(|| invalid("unknown local ASR model"))?;
        let files = match &entry.selector {
            ModelFileSelector::Native => {
                return Err(BackendError::new(
                    BackendErrorCode::Unsupported,
                    "native runtime manages this model install",
                ));
            }
            ModelFileSelector::Archive {
                url,
                size_bytes,
                sha256,
                ..
            } => vec![LocalAsrRemoteFile {
                path: url.clone(),
                local_path: archive_file_name(url),
                size_bytes: *size_bytes,
                sha256: Some(sha256.clone()),
            }],
            ModelFileSelector::QwenRepository | ModelFileSelector::Exact(_) => self
                .fetch_hf_manifest(
                    target.model_id(),
                    &entry.repository,
                    model_mirror_base(mirror)?,
                )
                .await?
                .files
                .into_iter()
                .map(|file| LocalAsrRemoteFile {
                    path: file.url,
                    local_path: Some(file.path),
                    size_bytes: file.size_bytes,
                    sha256: file.sha256,
                })
                .collect(),
        };
        Ok(LocalAsrRemoteInfo {
            target,
            mirror,
            total_bytes: files.iter().map(|file| file.size_bytes).sum(),
            files,
        })
    }

    pub async fn model_card(
        &self,
        target: LocalAsrTarget,
        mirror: crate::local_asr_catalog::LocalAsrMirror,
    ) -> Result<LocalAsrModelCard, BackendError> {
        let entry = self
            .catalog
            .find(target.runtime, target.model_id())
            .ok_or_else(|| invalid("unknown local ASR model"))?;
        if matches!(entry.selector, ModelFileSelector::Native) {
            return Err(BackendError::new(
                BackendErrorCode::Unsupported,
                "native runtime manages this model card",
            ));
        }
        let card = if entry.repository.is_empty() {
            ModelCard {
                model_id: target.model_id().into(),
                repository: String::new(),
                downloads: 0,
                likes: 0,
                description: entry.display_name.clone(),
            }
        } else {
            self.fetch_hf_model_card(
                target.model_id(),
                &entry.repository,
                model_mirror_base(mirror)?,
            )
            .await?
        };
        Ok(LocalAsrModelCard {
            target,
            mirror,
            downloads: card.downloads,
            likes: card.likes,
            description: card.description,
        })
    }

    pub async fn download_target(
        &self,
        target: LocalAsrTarget,
        mirror: crate::local_asr_catalog::LocalAsrMirror,
    ) -> Result<ModelCacheStatus, BackendError> {
        let entry = self
            .catalog
            .find(target.runtime, target.model_id())
            .cloned()
            .ok_or_else(|| invalid("unknown local ASR model"))?;
        let manifest = match entry.selector {
            ModelFileSelector::Native => {
                return Err(BackendError::new(
                    BackendErrorCode::Unsupported,
                    "native runtime manages this model install",
                ));
            }
            ModelFileSelector::Archive {
                url,
                root_dir,
                size_bytes,
                sha256,
                required_paths,
            } => {
                let file_path = archive_file_name(&url)
                    .ok_or_else(|| invalid("archive URL has no file name"))?;
                let mut manifest = ModelManifest::new(
                    target.model_id(),
                    "github-release",
                    vec![ModelFile {
                        path: file_path.clone(),
                        url,
                        size_bytes,
                        sha256: Some(sha256),
                    }],
                )?;
                manifest.archive = Some(ModelArchiveSpec {
                    file_path,
                    root_dir,
                    required_paths,
                });
                manifest
            }
            ModelFileSelector::QwenRepository | ModelFileSelector::Exact(_) => {
                self.fetch_hf_manifest(
                    target.model_id(),
                    &entry.repository,
                    model_mirror_base(mirror)?,
                )
                .await?
            }
        };
        self.download(manifest).await
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
        let entry = self
            .catalog
            .entries()
            .iter()
            .find(|entry| entry.target.model_id() == model_id && entry.repository == repository)
            .ok_or_else(|| invalid("model is not present in the Core catalog"))?;
        if matches!(
            entry.selector,
            ModelFileSelector::Native | ModelFileSelector::Archive { .. }
        ) {
            return Err(BackendError::new(
                BackendErrorCode::Unsupported,
                "selected model is not downloaded from a Hugging Face tree",
            ));
        }
        let mut pages = Vec::new();
        let mut url = format!("{base_url}/api/models/{repository}/tree/main?limit=1000");
        let mut seen_urls = BTreeSet::new();
        for page_index in 0..100 {
            if !seen_urls.insert(url.clone()) {
                return Err(invalid("model manifest pagination repeated a URL"));
            }
            let response = self
                .transport
                .request(ModelTransportRequest {
                    url: url.clone(),
                    range: None,
                    max_response_bytes: DEFAULT_MODEL_METADATA_BYTES,
                })
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
            let next = match response.metadata.link.as_deref() {
                Some(link) => next_hf_link(link, &url, base_url)?,
                None => None,
            };
            let Some(next) = next else {
                break;
            };
            if page_index == 99 {
                return Err(invalid("model manifest pagination exceeded the page limit"));
            }
            url = next;
        }
        manifest_from_hf_pages(entry, &pages, base_url, self.config.max_total_bytes)
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
                max_response_bytes: DEFAULT_MODEL_METADATA_BYTES,
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
        Ok(self.models_root_dir().join(model_id))
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
    ) -> Result<ModelCacheStatus, BackendError> {
        let progress_manifest = manifest.clone();
        let result = self.download_with_manifest(manifest).await;
        if let Err(error) = &result {
            if error.code != BackendErrorCode::Busy {
                self.emit(
                    &progress_manifest,
                    "",
                    progress_manifest.files.len(),
                    if error.code == BackendErrorCode::Cancelled {
                        ModelDownloadPhase::Cancelled
                    } else {
                        ModelDownloadPhase::Failed
                    },
                    None,
                    Some(error.message.clone()),
                );
            }
        }
        result
    }

    async fn download_with_manifest(
        &self,
        manifest: ModelManifest,
    ) -> Result<ModelCacheStatus, BackendError> {
        if manifest.total_bytes > self.config.max_total_bytes {
            return Err(invalid("model exceeds the configured total size limit"));
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let already_active = {
            let mut active = self
                .active_downloads
                .lock()
                .expect("model download lock poisoned");
            if active.contains_key(&manifest.model_id) {
                true
            } else {
                active.insert(manifest.model_id.clone(), Arc::clone(&cancelled));
                false
            }
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
            cancelled: Arc::clone(&cancelled),
        };
        if manifest
            .files
            .iter()
            .any(|file| file.size_bytes > self.config.max_file_bytes)
        {
            return Err(invalid("model file exceeds the configured size limit"));
        }
        let root = self.models_root_dir();
        std::fs::create_dir_all(&root).map_err(platform_error)?;
        let staging = root.join(format!(".{}.staging", manifest.model_id));
        std::fs::create_dir_all(&staging).map_err(platform_error)?;
        let mut partial = restore_partial_index(&staging, &manifest)?;
        self.emit(&manifest, "", 0, ModelDownloadPhase::Started, None, None);
        let mut downloaded_before: u64 = partial.files.values().copied().sum();
        for (file_index, file) in manifest.files.iter().enumerate() {
            if cancelled.load(Ordering::Acquire) {
                return Err(cancelled_error());
            }
            let path = staging.join(&file.path);
            validate_model_path(&file.path)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(platform_error)?;
            }
            let mut offset = partial.files.get(&file.path).copied().unwrap_or(0);
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
                            max_response_bytes: end - offset + 1,
                        })
                        .await
                    {
                        Ok(value) => {
                            match validate_range_response(&value, offset, end, file.size_bytes) {
                                Ok(()) => {
                                    response = Some(value);
                                    break;
                                }
                                Err(error) => last_error = Some(error.message),
                            }
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
                        let message = last_error.unwrap_or_else(|| "model download failed".into());
                        return Err(
                            BackendError::new(BackendErrorCode::Provider, message).retryable(true)
                        );
                    }
                };
                if cancelled.load(Ordering::Acquire) {
                    return Err(cancelled_error());
                }
                output.write_all(&response.bytes).map_err(platform_error)?;
                let received = response.bytes.len() as u64;
                offset = offset.saturating_add(received);
                downloaded_before = downloaded_before.saturating_add(received);
                partial.files.insert(file.path.clone(), offset);
                write_partial_index(&staging, &partial)?;
                self.emit(
                    &manifest,
                    &file.path,
                    file_index,
                    ModelDownloadPhase::Progress,
                    Some((downloaded_before, manifest.total_bytes)),
                    None,
                );
            }
            output.flush().map_err(platform_error)?;
            if let Some(expected) = &file.sha256 {
                let actual = sha256_file(&path)?;
                if !actual.eq_ignore_ascii_case(expected) {
                    let _ = std::fs::remove_file(&path);
                    return Err(BackendError::new(
                        BackendErrorCode::Provider,
                        format!("checksum mismatch for {}", file.path),
                    ));
                }
            }
            partial.files.insert(file.path.clone(), file.size_bytes);
            write_partial_index(&staging, &partial)?;
        }
        if let Some(archive) = &manifest.archive {
            expand_tar_bz2_archive(
                &staging,
                archive,
                self.config.max_file_bytes,
                self.config.max_total_bytes,
            )?;
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
            None,
        );
        if manifest.archive.is_some() {
            Ok(ModelCacheStatus {
                model_id: manifest.model_id.clone(),
                ready: destination.join(MODEL_READY_SENTINEL).is_file(),
                downloaded_bytes: manifest.total_bytes,
                expected_bytes: manifest.total_bytes,
            })
        } else {
            self.status(&manifest)
        }
    }

    pub fn cancel_download(&self, model_id: &str) -> Result<bool, BackendError> {
        validate_model_id(model_id)?;
        let active = self
            .active_downloads
            .lock()
            .expect("model download lock poisoned")
            .get(model_id)
            .cloned();
        if let Some(cancelled) = active {
            cancelled.store(true, Ordering::Release);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn emit(
        &self,
        manifest: &ModelManifest,
        file: &str,
        file_index: usize,
        phase: ModelDownloadPhase,
        bytes: Option<(u64, u64)>,
        error: Option<String>,
    ) {
        let sink = self
            .progress
            .read()
            .expect("model progress sink lock poisoned")
            .clone();
        let Some(sink) = sink else {
            return;
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_millis() as u64)
            .unwrap_or(0);
        let mut clocks = self
            .progress_clock
            .lock()
            .expect("model progress lock poisoned");
        let last = clocks.entry(manifest.model_id.clone()).or_default();
        if phase == ModelDownloadPhase::Progress && now.saturating_sub(*last) < 150 {
            return;
        }
        *last = now;
        let (downloaded, total) = bytes.unwrap_or((0, manifest.total_bytes));
        sink.publish(ModelDownloadProgress {
            runtime: self
                .catalog
                .entries()
                .iter()
                .find(|entry| entry.target.model_id() == manifest.model_id)
                .map(|entry| entry.target.runtime)
                .unwrap_or(LocalAsrRuntime::Generic),
            model_id: manifest.model_id.clone(),
            file: file.into(),
            file_index,
            file_count: manifest.files.len(),
            bytes_downloaded: downloaded,
            bytes_total: total,
            phase,
            error,
        });
    }

    pub fn cleanup_incomplete(&self, model_id: &str) -> Result<(), BackendError> {
        validate_model_id(model_id)?;
        let staging = self
            .models_root_dir()
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
        let root = self.models_root_dir();
        std::fs::create_dir_all(&root).map_err(platform_error)?;
        for entry in std::fs::read_dir(legacy_root).map_err(platform_error)? {
            let entry = entry.map_err(platform_error)?;
            let source = entry.path();
            let name = entry.file_name();
            let destination = root.join(&name);
            if destination.exists() {
                copy_dir_missing(&source, &destination).map_err(platform_error)?;
                migrate_ready_sentinel(&destination)?;
                continue;
            }
            std::fs::rename(&source, &destination)
                .or_else(|_| copy_dir_recursive(&source, &destination))
                .map_err(platform_error)?;
            migrate_ready_sentinel(&destination)?;
        }
        Ok(())
    }

    pub fn relocate_root(&self, next_root: PathBuf) -> Result<(), BackendError> {
        let next = ModelStoreConfig::new(next_root)?.models_root_dir;
        if !self
            .active_downloads
            .lock()
            .expect("model download lock poisoned")
            .is_empty()
        {
            return Err(BackendError::new(
                BackendErrorCode::Busy,
                "model downloads are still active",
            ));
        }
        let current = self.models_root_dir();
        if current != next {
            self.migrate_root_contents(&current, &next)?;
            *self
                .models_root_dir
                .write()
                .expect("model root lock poisoned") = next;
        }
        Ok(())
    }

    fn migrate_root_contents(&self, current: &Path, next: &Path) -> Result<(), BackendError> {
        if !current.is_dir() {
            std::fs::create_dir_all(next).map_err(platform_error)?;
            return Ok(());
        }
        std::fs::create_dir_all(next).map_err(platform_error)?;
        for entry in std::fs::read_dir(current).map_err(platform_error)? {
            let entry = entry.map_err(platform_error)?;
            let source = entry.path();
            let destination = next.join(entry.file_name());
            copy_dir_missing(&source, &destination).map_err(platform_error)?;
        }
        Ok(())
    }

    pub fn delete_model(&self, model_id: &str) -> Result<(), BackendError> {
        validate_model_id(model_id)?;
        if self.cancel_download(model_id)? {
            return Err(BackendError::new(
                BackendErrorCode::Busy,
                "model download cancellation is pending",
            ));
        }
        let directory = self.model_dir(model_id)?;
        if directory.exists() {
            std::fs::remove_dir_all(directory).map_err(platform_error)?;
        }
        self.cleanup_incomplete(model_id)
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

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PartialIndex {
    version: u8,
    files: BTreeMap<String, u64>,
}

fn restore_partial_index(
    staging: &Path,
    manifest: &ModelManifest,
) -> Result<PartialIndex, BackendError> {
    let index_path = staging.join(MODEL_PARTIAL_INDEX);
    let decoded = std::fs::read(&index_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<PartialIndex>(&bytes).ok());
    let mut valid = decoded.is_some();
    let partial = decoded.unwrap_or_else(|| PartialIndex {
        version: PARTIAL_INDEX_VERSION,
        files: BTreeMap::new(),
    });
    valid &= partial.version == PARTIAL_INDEX_VERSION;
    let expected = manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file.size_bytes))
        .collect::<BTreeMap<_, _>>();
    for (path, offset) in &partial.files {
        valid &= validate_model_path(path).is_ok();
        valid &= expected
            .get(path.as_str())
            .is_some_and(|size| offset <= size);
        valid &= std::fs::metadata(staging.join(path))
            .map(|metadata| metadata.is_file() && metadata.len() == *offset)
            .unwrap_or(false);
    }
    let mut staged_files = Vec::new();
    collect_relative_files(staging, staging, &mut staged_files).map_err(platform_error)?;
    for relative in staged_files {
        if relative == MODEL_PARTIAL_INDEX || relative == format!("{MODEL_PARTIAL_INDEX}.tmp") {
            continue;
        }
        valid &= partial.files.contains_key(&relative);
    }
    if valid {
        return Ok(partial);
    }
    std::fs::remove_dir_all(staging).map_err(platform_error)?;
    std::fs::create_dir_all(staging).map_err(platform_error)?;
    Ok(PartialIndex {
        version: PARTIAL_INDEX_VERSION,
        files: BTreeMap::new(),
    })
}

fn collect_relative_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<String>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            collect_relative_files(root, &entry.path(), files)?;
        } else {
            files.push(
                entry
                    .path()
                    .strip_prefix(root)
                    .expect("walked path stays below root")
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
    Ok(())
}

fn directory_size(path: &Path) -> std::io::Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    if path.is_file() {
        return Ok(std::fs::metadata(path)?.len());
    }
    std::fs::read_dir(path)?.try_fold(0u64, |total, entry| {
        let size = directory_size(&entry?.path())?;
        Ok(total.saturating_add(size))
    })
}

fn write_partial_index(staging: &Path, partial: &PartialIndex) -> Result<(), BackendError> {
    let path = staging.join(MODEL_PARTIAL_INDEX);
    let temporary = staging.join(format!("{MODEL_PARTIAL_INDEX}.tmp"));
    let bytes = serde_json::to_vec(partial)
        .map_err(|error| BackendError::new(BackendErrorCode::Internal, error.to_string()))?;
    let mut file = std::fs::File::create(&temporary).map_err(platform_error)?;
    file.write_all(&bytes).map_err(platform_error)?;
    file.sync_all().map_err(platform_error)?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(platform_error)?;
    }
    std::fs::rename(temporary, path).map_err(platform_error)
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

fn copy_dir_missing(source: &Path, destination: &Path) -> std::io::Result<()> {
    if source.is_dir() {
        std::fs::create_dir_all(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_dir_missing(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else if !destination.exists() {
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

fn validate_range_response(
    response: &ModelTransportResponse,
    start: u64,
    end: u64,
    total: u64,
) -> Result<(), BackendError> {
    let received = response.bytes.len() as u64;
    if response
        .metadata
        .content_length
        .is_some_and(|length| length != received)
    {
        return Err(invalid(
            "model response Content-Length does not match its body",
        ));
    }
    match response.status {
        200 if start == 0 && received == total => Ok(()),
        206 if received == end - start + 1 => match &response.metadata.content_range {
            Some(range) if range.start == start && range.end == end && range.total == total => {
                Ok(())
            }
            _ => Err(invalid(
                "model response Content-Range does not match the request",
            )),
        },
        status => Err(BackendError::new(
            BackendErrorCode::Provider,
            format!("unexpected model HTTP status or length: {status}"),
        )),
    }
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
    #[serde(default)]
    lfs: Option<HfLfs>,
}

#[derive(Debug, Deserialize)]
struct HfLfs {
    oid: String,
    size: u64,
}

pub fn parse_hf_tree_page(
    repository: &str,
    model_id: &str,
    entries: &[serde_json::Value],
) -> Result<Vec<ModelFile>, BackendError> {
    let catalog = ModelCatalog::standard();
    let entry = catalog
        .entries()
        .iter()
        .find(|entry| entry.target.model_id() == model_id && entry.repository == repository)
        .ok_or_else(|| invalid("model is not present in the Core catalog"))?;
    parse_hf_tree_page_for_entry(entry, entries, "https://huggingface.co")
}

/// Merge paginated Hugging Face tree responses while rejecting duplicate paths
/// across page boundaries.
pub fn merge_hf_tree_pages(
    repository: &str,
    model_id: &str,
    pages: &[Vec<serde_json::Value>],
) -> Result<Vec<ModelFile>, BackendError> {
    merge_hf_tree_pages_with_base(repository, model_id, pages, "https://huggingface.co")
}

pub fn merge_hf_tree_pages_with_base(
    repository: &str,
    model_id: &str,
    pages: &[Vec<serde_json::Value>],
    base_url: &str,
) -> Result<Vec<ModelFile>, BackendError> {
    let catalog = ModelCatalog::standard();
    let entry = catalog
        .entries()
        .iter()
        .find(|entry| entry.target.model_id() == model_id && entry.repository == repository)
        .ok_or_else(|| invalid("model is not present in the Core catalog"))?;
    merge_hf_tree_pages_for_entry(entry, pages, base_url)
}

fn manifest_from_hf_pages(
    entry: &ModelCatalogEntry,
    pages: &[Vec<serde_json::Value>],
    base_url: &str,
    max_total_bytes: u64,
) -> Result<ModelManifest, BackendError> {
    let files = merge_hf_tree_pages_for_entry(entry, pages, base_url)?;
    let manifest = ModelManifest::new(entry.target.model_id(), entry.repository.clone(), files)?;
    if manifest.total_bytes > max_total_bytes {
        return Err(invalid("model exceeds the configured total size limit"));
    }
    Ok(manifest)
}

fn merge_hf_tree_pages_for_entry(
    entry: &ModelCatalogEntry,
    pages: &[Vec<serde_json::Value>],
    base_url: &str,
) -> Result<Vec<ModelFile>, BackendError> {
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    for page in pages {
        for file in parse_hf_tree_page_for_entry(entry, page, base_url)? {
            if !seen.insert(file.path.clone()) {
                return Err(invalid("duplicate file across Hugging Face tree pages"));
            }
            files.push(file);
        }
    }
    if files.is_empty() {
        return Err(invalid(
            "Hugging Face tree returned no selected model files",
        ));
    }
    if let ModelFileSelector::Exact(expected) = &entry.selector {
        let actual = files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<BTreeSet<_>>();
        let missing = expected
            .iter()
            .find(|file| !actual.contains(file.local_path.as_str()));
        if let Some(missing) = missing {
            return Err(invalid(format!(
                "Hugging Face tree is missing required model file {}",
                missing.remote_path
            )));
        }
    }
    Ok(files)
}

fn parse_hf_tree_page_for_entry(
    catalog_entry: &ModelCatalogEntry,
    entries: &[serde_json::Value],
    base_url: &str,
) -> Result<Vec<ModelFile>, BackendError> {
    validate_model_id(catalog_entry.target.model_id())?;
    validate_repository(&catalog_entry.repository)?;
    let base_url = base_url.trim_end_matches('/');
    validate_model_url(&format!("{base_url}/"))?;
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    for value in entries {
        let entry: HfTreeEntry = serde_json::from_value(value.clone())
            .map_err(|_| invalid("invalid Hugging Face tree entry"))?;
        if entry.entry_type != "file" {
            continue;
        }
        validate_model_path(&entry.path)?;
        let Some(local_path) = catalog_entry.selector.local_path(&entry.path) else {
            continue;
        };
        validate_model_path(&local_path)?;
        if !seen.insert(local_path.clone()) {
            return Err(invalid("duplicate selected file in model tree"));
        }
        let (size_bytes, sha256) = match entry.lfs {
            Some(lfs) => (lfs.size, Some(parse_lfs_sha256(&lfs.oid)?)),
            None => (
                entry
                    .size
                    .ok_or_else(|| invalid("model file size is missing"))?,
                None,
            ),
        };
        if size_bytes == 0 || size_bytes > DEFAULT_MODEL_MAX_FILE_BYTES {
            return Err(invalid(
                "model file size is invalid or exceeds the configured limit",
            ));
        }
        files.push(ModelFile {
            url: format!(
                "{base_url}/{}/resolve/main/{}",
                catalog_entry.repository, entry.path
            ),
            path: local_path,
            size_bytes,
            sha256,
        });
    }
    Ok(files)
}

fn parse_lfs_sha256(oid: &str) -> Result<String, BackendError> {
    let digest = oid
        .strip_prefix("sha256:")
        .ok_or_else(|| invalid("unsupported Hugging Face LFS oid"))?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("invalid Hugging Face LFS sha256 oid"));
    }
    Ok(digest.to_ascii_lowercase())
}

fn qwen_model_file(path: &str) -> bool {
    const EXACT: &[&str] = &[
        "added_tokens.json",
        "chat_template.jinja",
        "config.json",
        "generation_config.json",
        "merges.txt",
        "model.safetensors",
        "model.safetensors.index.json",
        "preprocessor_config.json",
        "special_tokens_map.json",
        "tokenizer.json",
        "tokenizer_config.json",
        "vocab.json",
    ];
    let lower = path.to_ascii_lowercase();
    EXACT.contains(&lower.as_str())
        || (lower.starts_with("model-") && lower.ends_with(".safetensors"))
}

fn next_hf_link(
    header: &str,
    current_url: &str,
    base_url: &str,
) -> Result<Option<String>, BackendError> {
    let base = url::Url::parse(&format!("{}/", base_url.trim_end_matches('/')))
        .map_err(|_| invalid("invalid Hugging Face base URL"))?;
    let current =
        url::Url::parse(current_url).map_err(|_| invalid("invalid Hugging Face pagination URL"))?;
    for value in header.split(',') {
        let mut parts = value.trim().split(';');
        let target = parts.next().unwrap_or_default().trim();
        let is_next = parts.any(|part| {
            part.trim()
                .strip_prefix("rel=")
                .map(|rel| rel.trim_matches('"') == "next")
                .unwrap_or(false)
        });
        if !is_next {
            continue;
        }
        let target = target
            .strip_prefix('<')
            .and_then(|value| value.strip_suffix('>'))
            .ok_or_else(|| invalid("invalid Hugging Face Link header"))?;
        let next = current
            .join(target)
            .map_err(|_| invalid("invalid Hugging Face next-page URL"))?;
        if next.scheme() != base.scheme()
            || next.host_str() != base.host_str()
            || next.port_or_known_default() != base.port_or_known_default()
        {
            return Err(invalid("Hugging Face pagination changed origin"));
        }
        return Ok(Some(next.into()));
    }
    Ok(None)
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

fn expand_tar_bz2_archive(
    staging: &Path,
    spec: &ModelArchiveSpec,
    max_file_bytes: u64,
    max_total_bytes: u64,
) -> Result<(), BackendError> {
    validate_model_path(&spec.file_path)?;
    validate_model_path(&spec.root_dir)?;
    let extraction = staging.join(format!(
        ".archive-extract-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&extraction).map_err(platform_error)?;
    let mut extraction_guard = ArchiveStagingGuard {
        path: extraction.clone(),
        committed: false,
    };
    let archive_file =
        std::fs::File::open(staging.join(&spec.file_path)).map_err(platform_error)?;
    let decoder = bzip2::read::BzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(decoder);
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for entry in archive.entries().map_err(platform_error)? {
        let mut entry = entry.map_err(platform_error)?;
        let path = entry.path().map_err(platform_error)?.into_owned();
        let raw = path.to_string_lossy().replace('\\', "/");
        validate_model_path(&raw)?;
        let relative = path
            .strip_prefix(&spec.root_dir)
            .map_err(|_| invalid("model archive entry is outside its declared root"))?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let relative = relative.to_string_lossy().replace('\\', "/");
        validate_model_path(&relative)?;
        if matches!(
            relative.as_str(),
            MODEL_READY_SENTINEL | MODEL_PARTIAL_INDEX
        ) || !seen.insert(relative.clone())
        {
            return Err(invalid(
                "model archive contains a reserved or duplicate path",
            ));
        }
        let output = extraction.join(&relative);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(&output).map_err(platform_error)?;
            continue;
        }
        if !kind.is_file() {
            return Err(invalid(
                "model archive links and special files are not supported",
            ));
        }
        let size = entry.size();
        total = total
            .checked_add(size)
            .ok_or_else(|| invalid("model archive size overflowed"))?;
        if size > max_file_bytes || total > max_total_bytes {
            return Err(invalid("model archive exceeds the configured size limit"));
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).map_err(platform_error)?;
        }
        entry.unpack(&output).map_err(platform_error)?;
    }
    for required in &spec.required_paths {
        validate_model_path(required)?;
        if !extraction.join(required).exists() {
            return Err(invalid(format!(
                "model archive is missing required path {required}"
            )));
        }
    }
    std::fs::remove_file(staging.join(&spec.file_path)).map_err(platform_error)?;
    for entry in std::fs::read_dir(&extraction).map_err(platform_error)? {
        let entry = entry.map_err(platform_error)?;
        std::fs::rename(entry.path(), staging.join(entry.file_name())).map_err(platform_error)?;
    }
    std::fs::remove_dir(&extraction).map_err(platform_error)?;
    extraction_guard.committed = true;
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
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| invalid(error.to_string()))?;
        validate_model_path(entry.name())?;
        if matches!(entry.name(), MODEL_READY_SENTINEL | MODEL_PARTIAL_INDEX)
            || !seen.insert(entry.name().to_string())
        {
            return Err(invalid("archive contains a reserved or duplicate path"));
        }
        if entry.is_dir() {
            continue;
        }
        if entry.size() > max_file_bytes {
            return Err(invalid("archive entry exceeds the configured size limit"));
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| invalid("archive size overflowed"))?;
        if total > DEFAULT_MODEL_MAX_TOTAL_BYTES {
            return Err(invalid("archive exceeds the configured total size limit"));
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
    active: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    model_id: String,
    cancelled: Arc<AtomicBool>,
}

impl Drop for ActiveDownloadGuard {
    fn drop(&mut self) {
        let mut active = self.active.lock().expect("model download lock poisoned");
        if active
            .get(&self.model_id)
            .is_some_and(|current| Arc::ptr_eq(current, &self.cancelled))
        {
            active.remove(&self.model_id);
        }
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
fn archive_file_name(value: &str) -> Option<String> {
    url::Url::parse(value)
        .ok()?
        .path_segments()?
        .next_back()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
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
        ignore_range: bool,
    }

    struct BlockingTransport {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Semaphore>,
        body: Vec<u8>,
    }

    impl ModelTransport for BlockingTransport {
        fn request(
            &self,
            request: ModelTransportRequest,
        ) -> BoxFuture<'static, Result<ModelTransportResponse, BackendError>> {
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            let body = self.body.clone();
            Box::pin(async move {
                entered.notify_one();
                let permit = release.acquire_owned().await.unwrap();
                permit.forget();
                let (start, end) = request.range.unwrap();
                let bytes = body[start as usize..=end as usize].to_vec();
                Ok(ModelTransportResponse {
                    status: 206,
                    metadata: ModelHttpMetadata {
                        content_length: Some(bytes.len() as u64),
                        content_range: Some(ModelContentRange {
                            start,
                            end,
                            total: body.len() as u64,
                        }),
                        link: None,
                    },
                    bytes,
                })
            })
        }
    }
    impl ModelTransport for FakeTransport {
        fn request(
            &self,
            request: ModelTransportRequest,
        ) -> BoxFuture<'static, Result<ModelTransportResponse, BackendError>> {
            let calls = Arc::clone(&self.calls);
            let body = self.body.clone();
            let ignore_range = self.ignore_range;
            Box::pin(async move {
                calls.fetch_add(1, Ordering::Relaxed);
                let total = body.len() as u64;
                let (status, bytes, content_range) = match request.range {
                    Some(_) if ignore_range => (200, body, None),
                    Some((start, end)) => {
                        let bytes =
                            body[start as usize..=(end as usize).min(body.len() - 1)].to_vec();
                        (206, bytes, Some(ModelContentRange { start, end, total }))
                    }
                    None => (200, body, None),
                };
                assert!(bytes.len() as u64 <= request.max_response_bytes);
                Ok(ModelTransportResponse {
                    status,
                    metadata: ModelHttpMetadata {
                        content_length: Some(bytes.len() as u64),
                        content_range,
                        link: None,
                    },
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
    fn hf_tree_uses_catalog_selector_and_lfs_checksum() {
        let checksum = "a".repeat(64);
        let entries = vec![
            serde_json::json!({"type":"directory","path":"nested"}),
            serde_json::json!({"type":"file","path":"ggml-base.bin","size":3}),
            serde_json::json!({"type":"file","path":"ggml-small.bin","size":3,"lfs":{"oid":format!("sha256:{checksum}"),"size":4}}),
        ];
        let files = parse_hf_tree_page("ggerganov/whisper.cpp", "whisper-small", &entries).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "ggml-small.bin");
        assert_eq!(files[0].size_bytes, 4);
        assert_eq!(files[0].sha256.as_deref(), Some(checksum.as_str()));
        assert!(parse_hf_tree_page(
            "ggerganov/whisper.cpp",
            "whisper-small",
            &[serde_json::json!({"type":"file","path":"../x"})]
        )
        .is_err());
    }

    #[test]
    fn hf_link_pagination_accepts_same_origin_and_rejects_redirected_origin() {
        let current = "https://huggingface.co/api/models/org/model/tree/main?limit=1000";
        assert_eq!(
            next_hf_link(
                "<https://huggingface.co/api/models/org/model/tree/main?cursor=next>; rel=\"next\"",
                current,
                "https://huggingface.co",
            )
            .unwrap()
            .as_deref(),
            Some("https://huggingface.co/api/models/org/model/tree/main?cursor=next")
        );
        assert!(next_hf_link(
            "<https://evil.example/tree?cursor=next>; rel=\"next\"",
            current,
            "https://huggingface.co",
        )
        .is_err());
    }

    #[test]
    fn range_contract_accepts_complete_200_and_exact_206_only() {
        let complete = ModelTransportResponse {
            status: 200,
            bytes: vec![0; 4],
            metadata: ModelHttpMetadata {
                content_length: Some(4),
                ..ModelHttpMetadata::default()
            },
        };
        assert!(validate_range_response(&complete, 0, 3, 4).is_ok());
        let partial = ModelTransportResponse {
            status: 206,
            bytes: vec![0; 2],
            metadata: ModelHttpMetadata {
                content_length: Some(2),
                content_range: Some(ModelContentRange {
                    start: 2,
                    end: 3,
                    total: 4,
                }),
                link: None,
            },
        };
        assert!(validate_range_response(&partial, 2, 3, 4).is_ok());
        assert!(validate_range_response(&partial, 0, 1, 4).is_err());
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
            ignore_range: false,
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
        let staging = root.join(".demo.staging");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("weights.bin"), &body[..4]).unwrap();
        std::fs::write(
            staging.join(MODEL_PARTIAL_INDEX),
            br#"{"version":1,"files":{"weights.bin":4}}"#,
        )
        .unwrap();
        let status = store.download(manifest.clone()).await.unwrap();
        assert!(status.ready);
        assert_eq!(std::fs::read(root.join("demo/weights.bin")).unwrap(), body);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancellation_during_the_final_range_never_commits_ready() {
        let root =
            std::env::temp_dir().join(format!("openless-model-cancel-{}", uuid::Uuid::new_v4()));
        let body = b"0123".to_vec();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let store = Arc::new(ModelStore::with_transport(
            ModelStoreConfig::new(root.clone()).unwrap(),
            Arc::new(BlockingTransport {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                body: body.clone(),
            }),
        ));
        let manifest = ModelManifest::new(
            "cancelled",
            "org/cancelled",
            vec![ModelFile {
                path: "weights.bin".into(),
                url: "https://example.test/weights.bin".into(),
                size_bytes: body.len() as u64,
                sha256: Some(format!("{:x}", Sha256::digest(&body))),
            }],
        )
        .unwrap();
        let task = tokio::spawn({
            let store = Arc::clone(&store);
            async move { store.download(manifest).await }
        });
        entered.notified().await;
        assert!(store.cancel_download("cancelled").unwrap());
        release.add_permits(1);
        assert_eq!(
            task.await.unwrap().unwrap_err().code,
            BackendErrorCode::Cancelled
        );
        assert!(!root.join("cancelled").join(MODEL_READY_SENTINEL).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_partial_index_cleans_untrusted_staging_state() {
        let root = std::env::temp_dir().join(format!(
            "openless-model-corrupt-partial-{}",
            uuid::Uuid::new_v4()
        ));
        let staging = root.join(".demo.staging");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("weights.bin"), b"12").unwrap();
        std::fs::write(
            staging.join(MODEL_PARTIAL_INDEX),
            br#"{"version":1,"files":{"weights.bin":3}}"#,
        )
        .unwrap();
        let manifest = ModelManifest::new(
            "demo",
            "org/demo",
            vec![ModelFile {
                path: "weights.bin".into(),
                url: "https://example.test/weights.bin".into(),
                size_bytes: 4,
                sha256: None,
            }],
        )
        .unwrap();

        let partial = restore_partial_index(&staging, &manifest).unwrap();
        assert!(partial.files.is_empty());
        assert!(!staging.join("weights.bin").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn checksum_failure_never_commits_the_model() {
        let root =
            std::env::temp_dir().join(format!("openless-model-checksum-{}", uuid::Uuid::new_v4()));
        let body = b"wrong".to_vec();
        let store = ModelStore::with_transport(
            ModelStoreConfig::new(root.clone()).unwrap(),
            Arc::new(FakeTransport {
                calls: Arc::new(AtomicUsize::new(0)),
                body: body.clone(),
                ignore_range: false,
            }),
        );
        let progress = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&progress);
        store.set_progress_sink(Arc::new(move |event| {
            captured.lock().unwrap().push(event);
        }));
        let manifest = ModelManifest::new(
            "checksum",
            "org/checksum",
            vec![ModelFile {
                path: "weights.bin".into(),
                url: "https://example.test/weights.bin".into(),
                size_bytes: body.len() as u64,
                sha256: Some("0".repeat(64)),
            }],
        )
        .unwrap();

        assert!(store
            .download(manifest)
            .await
            .unwrap_err()
            .message
            .contains("checksum"));
        assert!(!root.join("checksum").exists());
        let terminal = progress.lock().unwrap().last().cloned().unwrap();
        assert_eq!(terminal.phase, ModelDownloadPhase::Failed);
        assert!(terminal.error.unwrap().contains("checksum"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_migration_merges_missing_files_and_preserves_destination() {
        let root =
            std::env::temp_dir().join(format!("openless-model-migrate-{}", uuid::Uuid::new_v4()));
        let current = root.join("current");
        let legacy = root.join("legacy");
        std::fs::create_dir_all(current.join("demo")).unwrap();
        std::fs::create_dir_all(legacy.join("demo")).unwrap();
        std::fs::write(current.join("demo/conflict.bin"), b"current").unwrap();
        std::fs::write(legacy.join("demo/conflict.bin"), b"legacy").unwrap();
        std::fs::write(legacy.join("demo/missing.bin"), b"missing").unwrap();
        std::fs::write(legacy.join("demo/.openless-asr-ready"), b"ready").unwrap();
        let store = ModelStore::new(ModelStoreConfig::new(current.clone()).unwrap()).unwrap();

        store.migrate_legacy_root(&legacy).unwrap();

        assert_eq!(
            std::fs::read(current.join("demo/conflict.bin")).unwrap(),
            b"current"
        );
        assert_eq!(
            std::fs::read(current.join("demo/missing.bin")).unwrap(),
            b"missing"
        );
        assert!(current.join("demo").join(MODEL_READY_SENTINEL).is_file());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn archive_extraction_rejects_parent_paths() {
        let root =
            std::env::temp_dir().join(format!("openless-model-archive-{}", uuid::Uuid::new_v4()));
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        archive
            .start_file("../escape", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"escape").unwrap();
        let bytes = archive.finish().unwrap().into_inner();
        assert!(extract_archive_safely(&bytes, &root, 1024).is_err());
        assert!(!root.with_file_name("escape").exists());
    }

    #[test]
    fn tar_archive_requires_declared_root_and_manifest_paths() {
        let staging =
            std::env::temp_dir().join(format!("openless-model-tar-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&staging).unwrap();
        let encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        header.set_mode(0o600);
        header.set_cksum();
        archive
            .append_data(&mut header, "fixture/model.onnx", &b"model"[..])
            .unwrap();
        let encoder = archive.into_inner().unwrap();
        let bytes = encoder.finish().unwrap();
        std::fs::write(staging.join("model.tar.bz2"), bytes).unwrap();
        let spec = ModelArchiveSpec {
            file_path: "model.tar.bz2".into(),
            root_dir: "fixture".into(),
            required_paths: vec!["model.onnx".into()],
        };

        expand_tar_bz2_archive(&staging, &spec, 1024, 2048).unwrap();

        assert_eq!(std::fs::read(staging.join("model.onnx")).unwrap(), b"model");
        assert!(!staging.join("model.tar.bz2").exists());
        let _ = std::fs::remove_dir_all(staging);
    }
}
