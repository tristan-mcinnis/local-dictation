//! Fixed-stack local transcription: Cohere Transcribe Q4 through native MLX.

use crate::audio::{drain_until_stopped, HeapAudioConsumer, SAMPLE_RATE};
use std::sync::{atomic::AtomicBool, Arc};

/// Legacy settings values kept only so an existing settings file/UI can migrate
/// without breaking. Runtime transcription is always Cohere MLX.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AsrBackend {
    Parakeet,
    #[default]
    Cohere,
    Moonshine,
    Whisper,
}

impl AsrBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parakeet => "parakeet",
            Self::Cohere => "cohere",
            Self::Moonshine => "moonshine",
            Self::Whisper => "whisper",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "parakeet" => Some(Self::Parakeet),
            "cohere" => Some(Self::Cohere),
            "moonshine" => Some(Self::Moonshine),
            "whisper" => Some(Self::Whisper),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CohereQuant {
    #[default]
    Int4,
    Int8,
}
impl CohereQuant {
    pub fn as_str(self) -> &'static str {
        if self == Self::Int4 {
            "int4"
        } else {
            "int8"
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "int4" | "4" => Some(Self::Int4),
            "int8" | "8" => Some(Self::Int8),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MoonshineSize {
    Tiny,
    #[default]
    Base,
}
impl MoonshineSize {
    pub fn as_str(self) -> &'static str {
        if self == Self::Tiny {
            "tiny"
        } else {
            "base"
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "tiny" => Some(Self::Tiny),
            "base" => Some(Self::Base),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AsrConfig {
    pub backend: AsrBackend,
    pub helper_path: String,
    pub model_dir: String,
    pub language: String,
    pub max_tokens: usize,
    pub parakeet_dir: String,
    pub cohere_dir: String,
    pub cohere_quant: CohereQuant,
    pub cohere_language: String,
    pub moonshine_dir: String,
    pub moonshine_size: MoonshineSize,
    pub whisper_model_path: String,
}

impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            backend: AsrBackend::Cohere,
            helper_path: crate::app_paths::cohere_mlx_helper_path(),
            model_dir: crate::app_paths::cohere_mlx_default_dir(),
            language: "en".into(),
            max_tokens: 256,
            parakeet_dir: String::new(),
            cohere_dir: crate::app_paths::cohere_mlx_default_dir(),
            cohere_quant: CohereQuant::Int4,
            cohere_language: "en".into(),
            moonshine_dir: String::new(),
            moonshine_size: MoonshineSize::Base,
            whisper_model_path: String::new(),
        }
    }
}

impl AsrConfig {
    pub fn describe(&self) -> String {
        format!("cohere-mlx/q4 ({}) · {}", self.language, self.model_dir)
    }
}

enum Backend {
    #[cfg(feature = "cohere-mlx")]
    Cohere(crate::cohere_mlx::CohereMlxWorker),
    Mock,
}

pub struct LocalInferenceWorker {
    backend: Backend,
}

fn normalize_disabled() -> bool {
    use std::sync::OnceLock;
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| std::env::var_os("DICTATE_NO_NORMALIZE").is_some())
}

impl LocalInferenceWorker {
    #[cfg(feature = "cohere-mlx")]
    pub fn initialize(cfg: &AsrConfig) -> eyre::Result<Self> {
        let worker =
            crate::cohere_mlx::CohereMlxWorker::start(crate::cohere_mlx::CohereMlxConfig {
                helper_path: cfg.helper_path.clone(),
                model_dir: cfg.model_dir.clone(),
                language: cfg.language.clone(),
                max_tokens: cfg.max_tokens,
            })?;
        Ok(Self {
            backend: Backend::Cohere(worker),
        })
    }

    #[cfg(not(feature = "cohere-mlx"))]
    pub fn initialize(_cfg: &AsrConfig) -> eyre::Result<Self> {
        Err(eyre::eyre!(
            "Cohere MLX support is not compiled; use --features cohere-mlx"
        ))
    }

    pub fn initialize_mock() -> Self {
        Self {
            backend: Backend::Mock,
        }
    }

    pub async fn run_inference_pipeline(
        &mut self,
        consumer: HeapAudioConsumer,
        is_recording: Arc<AtomicBool>,
    ) -> eyre::Result<String> {
        let audio_buffer = drain_until_stopped(consumer, is_recording).await;
        self.transcribe_pcm(&audio_buffer)
    }

    pub fn transcribe_pcm(&mut self, audio: &[f32]) -> eyre::Result<String> {
        if audio.is_empty() {
            return Ok(String::new());
        }
        let mut buffer = audio.to_vec();
        if !normalize_disabled() {
            crate::audio::normalize_for_asr(&mut buffer);
        }
        match &mut self.backend {
            #[cfg(feature = "cohere-mlx")]
            Backend::Cohere(worker) => worker.transcribe(&buffer),
            Backend::Mock => Ok(format!(
                "<mock transcript of {} samples @ {} Hz>",
                buffer.len(),
                SAMPLE_RATE
            )),
        }
    }
}

#[cfg(feature = "cohere-mlx")]
pub fn load_wav_mono16k<P: AsRef<std::path::Path>>(path: P) -> eyre::Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    if spec.channels != 1 || spec.sample_rate != SAMPLE_RATE {
        return Err(eyre::eyre!(
            "expected mono {} Hz WAV, got {} channels at {} Hz",
            SAMPLE_RATE,
            spec.channels,
            spec.sample_rate
        ));
    }
    match spec.sample_format {
        hound::SampleFormat::Float => Ok(reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?),
        hound::SampleFormat::Int => Ok(reader
            .samples::<i16>()
            .map(|sample| sample.map(|value| value as f32 / 32768.0))
            .collect::<Result<Vec<_>, _>>()?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_stack_description_is_explicit() {
        let cfg = AsrConfig::default();
        assert!(cfg.describe().contains("cohere-mlx/q4"));
    }
}
