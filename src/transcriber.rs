use crate::audio::{drain_until_stopped, HeapAudioConsumer, SAMPLE_RATE};
use std::sync::{atomic::AtomicBool, Arc};

#[cfg(feature = "parakeet")]
use parakeet_rs::{ParakeetTDT, TimestampMode, Transcriber};

/// Which on-device ASR engine turns audio into raw text.
///
/// - **Parakeet** (default): NVIDIA Parakeet TDT v3, a CTC/TDT model run via
///   `parakeet-rs`. Lowest latency — a single forward pass, no autoregressive
///   decode. This is the proven push-to-talk path.
/// - **Cohere**: Cohere Transcribe (2B Conformer encoder + Transformer decoder)
///   run via `transcribe-rs`. Higher accuracy and 14 languages, at the cost of
///   a token-by-token decode (slower per utterance). Opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AsrBackend {
    #[default]
    Parakeet,
    Cohere,
}

impl AsrBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            AsrBackend::Parakeet => "parakeet",
            AsrBackend::Cohere => "cohere",
        }
    }

    /// Parse a backend name (case-insensitive). Returns `None` for unknown
    /// values so callers can fall back to the default.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "parakeet" => Some(Self::Parakeet),
            "cohere" => Some(Self::Cohere),
            _ => None,
        }
    }
}

/// Cohere ONNX weight quantization. `int4` is the dictation default — smaller
/// on disk and faster through the autoregressive decoder; `int8` trades some
/// latency for a little accuracy. (Ignored when the Parakeet backend is used.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CohereQuant {
    #[default]
    Int4,
    Int8,
}

impl CohereQuant {
    pub fn as_str(self) -> &'static str {
        match self {
            CohereQuant::Int4 => "int4",
            CohereQuant::Int8 => "int8",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "int4" | "4" => Some(Self::Int4),
            "int8" | "8" => Some(Self::Int8),
            _ => None,
        }
    }
}

/// Fully-resolved ASR selection: which backend to run and where each model
/// lives. Built once at boot from `settings.json` + env + `app_paths` (see
/// `settings::Settings::resolve_asr` and `main::asr_config`), then handed to
/// [`LocalInferenceWorker::initialize`]. Only the fields for the chosen
/// `backend` need to be valid.
#[derive(Debug, Clone)]
pub struct AsrConfig {
    pub backend: AsrBackend,
    /// Parakeet model directory (encoder/decoder ONNX + vocab).
    pub parakeet_dir: String,
    /// Cohere model directory (encoder/decoder ONNX + tokens.txt).
    pub cohere_dir: String,
    pub cohere_quant: CohereQuant,
    /// Language hint for the Cohere decoder (it has no auto-detect). BCP-47-ish
    /// code, e.g. `en`, `de`, `zh`, `ja`. Defaults to `en`.
    pub cohere_language: String,
}

impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            backend: AsrBackend::default(),
            parakeet_dir: String::new(),
            cohere_dir: String::new(),
            cohere_quant: CohereQuant::default(),
            cohere_language: "en".to_string(),
        }
    }
}

impl AsrConfig {
    /// One-line human description of the active backend + its model path, for
    /// boot/bench logging.
    pub fn describe(&self) -> String {
        match self.backend {
            AsrBackend::Parakeet => format!("parakeet · {}", self.parakeet_dir),
            AsrBackend::Cohere => format!(
                "cohere/{} ({}) · {}",
                self.cohere_quant.as_str(),
                self.cohere_language,
                self.cohere_dir
            ),
        }
    }
}

/// The loaded model behind [`LocalInferenceWorker`]. Each real variant is
/// gated on its Cargo feature; `Mock` covers the featureless/test build (and
/// is the only variant when neither ASR backend is compiled in).
enum Backend {
    #[cfg(feature = "parakeet")]
    Parakeet(ParakeetTDT),
    #[cfg(feature = "cohere")]
    Cohere {
        model: transcribe_rs::onnx::cohere::CohereModel,
        params: transcribe_rs::onnx::cohere::CohereParams,
    },
    /// Featureless/test build only — no real backend compiled in.
    #[cfg(not(any(feature = "parakeet", feature = "cohere")))]
    Mock,
}

pub struct LocalInferenceWorker {
    backend: Backend,
}

impl LocalInferenceWorker {
    /// Load the ASR model selected by `cfg`. Available whenever at least one
    /// real backend feature is compiled in. Errors clearly if the selected
    /// backend's feature was not built.
    #[cfg(any(feature = "parakeet", feature = "cohere"))]
    pub fn initialize(cfg: &AsrConfig) -> eyre::Result<Self> {
        let backend = match cfg.backend {
            AsrBackend::Parakeet => Self::load_parakeet(&cfg.parakeet_dir)?,
            AsrBackend::Cohere => Self::load_cohere(cfg)?,
        };
        Ok(Self { backend })
    }

    #[cfg(feature = "parakeet")]
    fn load_parakeet(model_dir: &str) -> eyre::Result<Backend> {
        let model = ParakeetTDT::from_pretrained(model_dir, None)
            .map_err(|e| eyre::eyre!("ParakeetTDT::from_pretrained failed: {e:?}"))?;
        Ok(Backend::Parakeet(model))
    }

    #[cfg(all(not(feature = "parakeet"), feature = "cohere"))]
    fn load_parakeet(_model_dir: &str) -> eyre::Result<Backend> {
        Err(eyre::eyre!(
            "ASR backend 'parakeet' was selected, but this binary was built \
             without the `parakeet` feature. Rebuild with --features parakeet \
             (or full), or set the backend to 'cohere'."
        ))
    }

    #[cfg(feature = "cohere")]
    fn load_cohere(cfg: &AsrConfig) -> eyre::Result<Backend> {
        use transcribe_rs::onnx::cohere::{CohereModel, CohereParams};
        use transcribe_rs::onnx::Quantization;
        use transcribe_rs::OrtAccelerator;

        let quant = match cfg.cohere_quant {
            CohereQuant::Int4 => Quantization::Int4,
            CohereQuant::Int8 => Quantization::Int8,
        };
        let path = std::path::Path::new(&cfg.cohere_dir);

        // `set_ort_accelerator` is a transcribe-rs process-global read at load
        // time, so set it immediately before each `CohereModel::load` attempt.
        let load_with = |accel: OrtAccelerator| -> Result<CohereModel, _> {
            transcribe_rs::set_ort_accelerator(accel);
            CohereModel::load(path, &quant)
        };

        // The Cohere ONNX export stores its weights in an external-data sidecar
        // (`*.onnx.data`). onnxruntime's CoreML EP currently can't resolve that
        // sidecar (it mis-joins the path and fails fast at session creation),
        // whereas the CPU EP loads it fine. So: honour an explicit
        // `DICTATE_COHERE_ACCEL` override verbatim; otherwise try CoreML for the
        // acceleration and **fall back to CPU** if (as today) it can't load.
        let forced = std::env::var("DICTATE_COHERE_ACCEL").ok().and_then(|v| {
            match v.trim().to_ascii_lowercase().as_str() {
                "coreml" | "core_ml" => Some(OrtAccelerator::CoreMl),
                "cpu" | "cpu_only" => Some(OrtAccelerator::CpuOnly),
                "auto" => Some(OrtAccelerator::Auto),
                _ => None,
            }
        });

        let model = if let Some(accel) = forced {
            load_with(accel).map_err(|e| {
                eyre::eyre!(
                    "CohereModel::load failed (accel={accel:?}, {}): {e:?}",
                    cfg.cohere_dir
                )
            })?
        } else {
            match load_with(OrtAccelerator::CoreMl) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!(
                        "[asr] Cohere CoreML load failed ({e}); falling back to CPU \
                         (set DICTATE_COHERE_ACCEL=cpu to silence this)"
                    );
                    load_with(OrtAccelerator::CpuOnly).map_err(|e2| {
                        eyre::eyre!(
                            "CohereModel::load failed on CPU fallback ({}): {e2:?}",
                            cfg.cohere_dir
                        )
                    })?
                }
            }
        };

        let params = CohereParams {
            language: Some(cfg.cohere_language.clone()),
            translate: false,
            max_new_tokens: None,
        };
        Ok(Backend::Cohere { model, params })
    }

    #[cfg(all(not(feature = "cohere"), feature = "parakeet"))]
    fn load_cohere(_cfg: &AsrConfig) -> eyre::Result<Backend> {
        Err(eyre::eyre!(
            "ASR backend 'cohere' was selected, but this binary was built \
             without the `cohere` feature. Rebuild with --features cohere \
             (or full), or set the backend back to 'parakeet'."
        ))
    }

    #[cfg(not(any(feature = "parakeet", feature = "cohere")))]
    pub fn initialize_mock() -> Self {
        Self {
            backend: Backend::Mock,
        }
    }

    #[cfg(any(feature = "parakeet", feature = "cohere"))]
    pub fn initialize_mock() -> Self {
        panic!("initialize_mock is unavailable when built with an ASR backend feature; call initialize(&AsrConfig) instead");
    }

    /// Drain the SPSC ring buffer until capture stops, then run inference on
    /// the accumulated samples. The drain/termination logic lives in
    /// `audio::drain_until_stopped`; this just composes it with the model.
    pub async fn run_inference_pipeline(
        &mut self,
        consumer: HeapAudioConsumer,
        is_recording: Arc<AtomicBool>,
    ) -> eyre::Result<String> {
        let audio_buffer = drain_until_stopped(consumer, is_recording).await;
        self.transcribe_pcm(&audio_buffer)
    }

    /// Synchronously transcribe a slice of 16 kHz mono f32 samples.
    /// Mock build returns a deterministic placeholder string.
    pub fn transcribe_pcm(&mut self, audio: &[f32]) -> eyre::Result<String> {
        if audio.is_empty() {
            return Ok(String::new());
        }
        match &mut self.backend {
            #[cfg(feature = "parakeet")]
            Backend::Parakeet(model) => {
                let result = model
                    .transcribe_samples(
                        audio.to_vec(),
                        SAMPLE_RATE,
                        1,
                        Some(TimestampMode::Sentences),
                    )
                    .map_err(|e| eyre::eyre!("transcribe_samples failed: {e:?}"))?;
                Ok(result.text)
            }
            #[cfg(feature = "cohere")]
            Backend::Cohere { model, params } => {
                let result = model
                    .transcribe_with(audio, params)
                    .map_err(|e| eyre::eyre!("cohere transcribe_with failed: {e:?}"))?;
                Ok(result.text)
            }
            #[cfg(not(any(feature = "parakeet", feature = "cohere")))]
            Backend::Mock => Ok(format!(
                "<mock transcript of {} samples @ {} Hz>",
                audio.len(),
                SAMPLE_RATE
            )),
        }
    }
}

/// WAV loader (mono 16 kHz expected; int16 PCM auto-converted to f32).
/// Available with either ASR backend (both pull `hound`).
#[cfg(any(feature = "parakeet", feature = "cohere"))]
pub fn load_wav_mono16k<P: AsRef<std::path::Path>>(path: P) -> eyre::Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    if spec.channels != 1 {
        return Err(eyre::eyre!(
            "expected mono WAV, got {} channels",
            spec.channels
        ));
    }
    if spec.sample_rate != SAMPLE_RATE {
        return Err(eyre::eyre!(
            "expected {} Hz, got {} Hz",
            SAMPLE_RATE,
            spec.sample_rate
        ));
    }
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|r| r.map(|s| s as f32 / 32768.0))
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asr_backend_parse_roundtrip() {
        assert_eq!(AsrBackend::parse("parakeet"), Some(AsrBackend::Parakeet));
        assert_eq!(AsrBackend::parse("  Cohere "), Some(AsrBackend::Cohere));
        assert_eq!(AsrBackend::parse("COHERE"), Some(AsrBackend::Cohere));
        assert_eq!(AsrBackend::parse("whisper"), None);
        assert_eq!(AsrBackend::Parakeet.as_str(), "parakeet");
        assert_eq!(AsrBackend::Cohere.as_str(), "cohere");
        // default is the proven, lowest-latency backend
        assert_eq!(AsrBackend::default(), AsrBackend::Parakeet);
    }

    #[test]
    fn cohere_quant_parse_roundtrip() {
        assert_eq!(CohereQuant::parse("int4"), Some(CohereQuant::Int4));
        assert_eq!(CohereQuant::parse("8"), Some(CohereQuant::Int8));
        assert_eq!(CohereQuant::parse("fp16"), None);
        assert_eq!(CohereQuant::Int4.as_str(), "int4");
        assert_eq!(CohereQuant::default(), CohereQuant::Int4);
    }

    #[test]
    fn describe_reflects_backend() {
        let mut cfg = AsrConfig {
            backend: AsrBackend::Parakeet,
            parakeet_dir: "/m/parakeet".into(),
            cohere_dir: "/m/cohere-int4".into(),
            cohere_quant: CohereQuant::Int4,
            cohere_language: "en".into(),
        };
        assert!(cfg.describe().contains("parakeet"));
        cfg.backend = AsrBackend::Cohere;
        let d = cfg.describe();
        assert!(d.contains("cohere/int4"));
        assert!(d.contains("/m/cohere-int4"));
    }
}
