use crate::audio::{HeapAudioConsumer, SAMPLE_RATE};
use ringbuf::traits::{Consumer, Observer};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[cfg(feature = "parakeet")]
use parakeet_rs::{ParakeetTDT, TimestampMode, Transcriber};

pub struct LocalInferenceWorker {
    #[cfg(feature = "parakeet")]
    model: ParakeetTDT,
    #[cfg(not(feature = "parakeet"))]
    _mock: (),
}

impl LocalInferenceWorker {
    #[cfg(feature = "parakeet")]
    pub fn initialize<P: AsRef<std::path::Path>>(model_dir: P) -> eyre::Result<Self> {
        let model = ParakeetTDT::from_pretrained(model_dir, None)
            .map_err(|e| eyre::eyre!("ParakeetTDT::from_pretrained failed: {e:?}"))?;
        Ok(Self { model })
    }

    #[cfg(not(feature = "parakeet"))]
    pub fn initialize_mock() -> Self {
        Self { _mock: () }
    }

    #[cfg(feature = "parakeet")]
    pub fn initialize_mock() -> Self {
        panic!("initialize_mock is unavailable when built with the `parakeet` feature; call initialize(path) instead");
    }

    /// Drain the SPSC ring buffer until capture stops, then run inference on
    /// the accumulated samples.
    pub async fn run_inference_pipeline(
        &mut self,
        mut consumer: HeapAudioConsumer,
        is_recording: Arc<AtomicBool>,
    ) -> eyre::Result<String> {
        let mut audio_buffer: Vec<f32> = Vec::with_capacity(SAMPLE_RATE as usize * 5);

        while is_recording.load(Ordering::SeqCst) || !consumer.is_empty() {
            let pending = consumer.occupied_len();
            if pending > 0 {
                let mut chunk = vec![0.0_f32; pending];
                let got = consumer.pop_slice(&mut chunk);
                audio_buffer.extend_from_slice(&chunk[..got]);
            } else {
                tokio::time::sleep(tokio::time::Duration::from_millis(15)).await;
            }
        }

        self.transcribe_pcm(&audio_buffer)
    }

    /// Synchronously transcribe a slice of 16 kHz mono f32 samples.
    /// Mock build returns a deterministic placeholder string.
    pub fn transcribe_pcm(&mut self, audio: &[f32]) -> eyre::Result<String> {
        if audio.is_empty() {
            return Ok(String::new());
        }
        #[cfg(feature = "parakeet")]
        {
            let result = self
                .model
                .transcribe_samples(
                    audio.to_vec(),
                    SAMPLE_RATE,
                    1,
                    Some(TimestampMode::Sentences),
                )
                .map_err(|e| eyre::eyre!("transcribe_samples failed: {e:?}"))?;
            Ok(result.text)
        }
        #[cfg(not(feature = "parakeet"))]
        {
            Ok(format!(
                "<mock transcript of {} samples @ {} Hz>",
                audio.len(),
                SAMPLE_RATE
            ))
        }
    }
}

/// WAV loader (mono 16 kHz expected; int16 PCM auto-converted to f32).
/// Only available when the `parakeet` feature is on (pulls hound as a dep).
#[cfg(feature = "parakeet")]
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
