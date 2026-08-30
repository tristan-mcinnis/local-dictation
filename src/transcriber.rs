//! Fixed-stack local transcription: Cohere Transcribe Q4 through native MLX.

use crate::audio::{drain_until_stopped, HeapAudioConsumer, SAMPLE_RATE};
use std::sync::{atomic::AtomicBool, Arc};

/// The fixed Cohere MLX ASR configuration. This is the only runtime ASR
/// selection; the old Parakeet/Moonshine/Whisper backends and the Cohere
/// quant-choice plumbing were removed with the migration shims.
#[derive(Debug, Clone)]
pub struct AsrConfig {
    /// Native MLX helper binary path.
    pub helper_path: String,
    /// Fixed Cohere Q4 checkpoint directory.
    pub model_dir: String,
    /// Decoder language hint (e.g. `"en"`). Override: `DICTATE_COHERE_LANG`.
    pub language: String,
    /// Max tokens the decoder emits per utterance.
    pub max_tokens: usize,
}

impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            helper_path: crate::app_paths::cohere_mlx_helper_path(),
            model_dir: crate::app_paths::cohere_mlx_default_dir(),
            language: "en".into(),
            max_tokens: 256,
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
