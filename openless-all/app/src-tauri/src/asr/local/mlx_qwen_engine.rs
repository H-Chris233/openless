//! qwen3_asr_rs 的 MLX/Metal 包装。
//!
//! 上游库目前以音频文件作为输入。OpenLess 的录音器产生的是 16 kHz、单声道、
//! 16-bit PCM，因此这里只做一次临时 WAV 封装；模型本身保持驻留并跨会话复用。

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use qwen3_asr_rs::inference::AsrInference;
use qwen3_asr_rs::tensor::Device;

pub struct MlxQwenAsrEngine {
    inference: Mutex<AsrInference>,
}

impl MlxQwenAsrEngine {
    pub fn load(model_dir: &Path) -> Result<Self> {
        ensure_tokenizer_json(model_dir)?;
        // qwen3_asr_rs 的 CLI 会在加载模型前做这一步；OpenLess 直接调用库 API，
        // 必须自行初始化全局 MLX stream，否则首次创建张量会 panic。
        qwen3_asr_rs::backend::mlx::stream::init_mlx(true);
        log::info!(
            "[local-qwen3-mlx] loading model from {}",
            model_dir.display()
        );
        let inference = AsrInference::load(model_dir, Device::gpu())
            .with_context(|| format!("加载 Qwen3-ASR MLX 模型失败: {}", model_dir.display()))?;
        Ok(Self {
            inference: Mutex::new(inference),
        })
    }

    pub fn transcribe_pcm(&self, samples: &[f32]) -> Result<String> {
        let path =
            std::env::temp_dir().join(format!("openless-qwen3-{}.wav", uuid::Uuid::new_v4()));
        let pcm: Vec<i16> = samples
            .iter()
            .map(|sample| (sample.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect();
        std::fs::write(&path, crate::asr::wav::encode_wav_16k_mono(&pcm))
            .with_context(|| format!("写入临时 Qwen3-ASR 音频失败: {}", path.display()))?;

        let path_string = path.to_string_lossy().into_owned();
        let result = self
            .inference
            .lock()
            .map_err(|_| anyhow::anyhow!("Qwen3-ASR MLX 引擎锁已中毒"))?
            .transcribe(&path_string, None)
            .context("Qwen3-ASR MLX batch 解码失败");
        let _ = std::fs::remove_file(&path);
        result.map(|output| output.text.trim().to_string())
    }
}

/// Qwen 官方 ASR 权重通常只有 `vocab.json` + `merges.txt`，而 qwen3_asr_rs
/// 使用 HuggingFace 的统一 `tokenizer.json`。这里在首次加载时本地生成一次，
/// 避免要求用户安装 Python/Transformers；如果模型包已经带 tokenizer.json，则直接复用。
fn ensure_tokenizer_json(model_dir: &Path) -> Result<()> {
    let tokenizer_path = model_dir.join("tokenizer.json");
    if tokenizer_path.is_file() {
        return Ok(());
    }
    let vocab = model_dir.join("vocab.json");
    let merges = model_dir.join("merges.txt");
    if !vocab.is_file() || !merges.is_file() {
        anyhow::bail!(
            "Qwen3-ASR MLX 模型缺少 tokenizer.json、vocab.json 或 merges.txt: {}",
            model_dir.display()
        );
    }
    let vocab = vocab
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Qwen3-ASR vocab 路径不是有效 UTF-8"))?;
    let merges = merges
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Qwen3-ASR merges 路径不是有效 UTF-8"))?;
    let model = tokenizers::models::bpe::BPE::from_file(vocab, merges)
        .build()
        .map_err(|error| anyhow::anyhow!("生成 Qwen3-ASR BPE tokenizer 失败: {error}"))?;
    let mut tokenizer = tokenizers::Tokenizer::new(model);
    tokenizer.with_pre_tokenizer(Some(
        tokenizers::pre_tokenizers::byte_level::ByteLevel::default(),
    ));
    tokenizer.with_decoder(Some(tokenizers::decoders::byte_level::ByteLevel::default()));
    let temporary = tokenizer_path.with_extension("json.partial");
    let tokenizer_json = tokenizer
        .to_string(false)
        .map_err(|error| anyhow::anyhow!("序列化 Qwen3-ASR tokenizer 失败: {error}"))?;
    std::fs::write(&temporary, tokenizer_json)
        .with_context(|| format!("写入 Qwen3-ASR tokenizer 失败: {}", temporary.display()))?;
    std::fs::rename(&temporary, &tokenizer_path).with_context(|| {
        format!(
            "提交 Qwen3-ASR tokenizer 失败: {} -> {}",
            temporary.display(),
            tokenizer_path.display()
        )
    })?;
    Ok(())
}
