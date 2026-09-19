//! Persistent client for the native Cohere Transcribe MLX helper.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

#[derive(Debug, Clone)]
pub struct CohereMlxConfig {
    pub helper_path: String,
    pub model_dir: String,
    pub language: String,
    pub max_tokens: usize,
}

#[derive(Serialize)]
struct Request<'a> {
    op: &'a str,
    request_id: String,
    sample_rate: u32,
    channels: u8,
    language: &'a str,
    max_tokens: usize,
    audio_f32_base64: String,
}

#[derive(Debug, Deserialize)]
struct Response {
    ok: bool,
    event: Option<String>,
    request_id: Option<String>,
    text: Option<String>,
    error_code: Option<String>,
    error_message: Option<String>,
    timings_ms: Option<std::collections::HashMap<String, f64>>,
}

pub struct CohereMlxWorker {
    config: CohereMlxConfig,
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    sequence: u64,
}

impl CohereMlxWorker {
    pub fn start(config: CohereMlxConfig) -> eyre::Result<Self> {
        let mut child = Command::new(&config.helper_path)
            .args([
                "--model-dir",
                &config.model_dir,
                "--language",
                &config.language,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| eyre::eyre!("start Cohere MLX helper {}: {e}", config.helper_path))?;
        let stdin = BufWriter::new(
            child
                .stdin
                .take()
                .ok_or_else(|| eyre::eyre!("helper stdin unavailable"))?,
        );
        let mut stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| eyre::eyre!("helper stdout unavailable"))?,
        );
        let ready = read_response(&mut stdout)?;
        if !ready.ok || ready.event.as_deref() != Some("ready") {
            return Err(response_error(
                "Cohere MLX helper did not become ready",
                &ready,
            ));
        }
        Ok(Self {
            config,
            child,
            stdin,
            stdout,
            sequence: 0,
        })
    }

    pub fn transcribe(&mut self, audio: &[f32]) -> eyre::Result<String> {
        self.sequence += 1;
        let request_id = self.sequence.to_string();
        let mut bytes = Vec::with_capacity(audio.len() * 4);
        for sample in audio {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let request = Request {
            op: "transcribe",
            request_id: request_id.clone(),
            sample_rate: 16_000,
            channels: 1,
            language: &self.config.language,
            max_tokens: self.config.max_tokens,
            audio_f32_base64: STANDARD.encode(bytes),
        };
        serde_json::to_writer(&mut self.stdin, &request)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;

        let response = read_response(&mut self.stdout)?;
        check_response(&response, &request_id)?;
        if let Some(timings) = &response.timings_ms {
            if let Some(total) = timings.get("total") {
                eprintln!("[cohere] helper inference {total:.1} ms");
            }
        }
        Ok(response.text.unwrap_or_default())
    }
}

impl Drop for CohereMlxWorker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_response(reader: &mut BufReader<ChildStdout>) -> eyre::Result<Response> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(eyre::eyre!("Cohere MLX helper exited unexpectedly"));
    }
    serde_json::from_str(&line).map_err(|e| eyre::eyre!("invalid Cohere MLX response: {e}: {line}"))
}

/// Validate a transcribe reply. The helper's own error is reported FIRST: an
/// error reply used to arrive without a `request_id`, and checking the id first
/// turned every real failure into "response id mismatch". Requests are strictly
/// one-in, one-out, so a failure line is this request's reply either way.
fn check_response(response: &Response, request_id: &str) -> eyre::Result<()> {
    if !response.ok {
        return Err(response_error("Cohere MLX transcription failed", response));
    }
    if response.request_id.as_deref() != Some(request_id) {
        return Err(eyre::eyre!(
            "Cohere MLX response id mismatch (sent {request_id}, got {:?})",
            response.request_id
        ));
    }
    Ok(())
}

fn response_error(context: &str, response: &Response) -> eyre::Report {
    eyre::eyre!(
        "{context}: {}: {}",
        response.error_code.as_deref().unwrap_or("unknown"),
        response.error_message.as_deref().unwrap_or("no detail")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_stable_ndjson() {
        let request = Request {
            op: "transcribe",
            request_id: "7".into(),
            sample_rate: 16_000,
            channels: 1,
            language: "en",
            max_tokens: 64,
            audio_f32_base64: STANDARD.encode(1.0_f32.to_le_bytes()),
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["sample_rate"], 16_000);
        assert_eq!(value["request_id"], "7");
    }

    #[test]
    fn a_helper_error_is_reported_even_without_a_request_id() {
        // Regression: this reply used to surface as "response id mismatch".
        let reply: Response = serde_json::from_str(
            r#"{"ok":false,"error_code":"model","error_message":"audio too short"}"#,
        )
        .unwrap();
        let err = check_response(&reply, "7").unwrap_err().to_string();
        assert!(err.contains("model") && err.contains("audio too short"), "{err}");
        assert!(!err.contains("mismatch"), "{err}");
    }

    #[test]
    fn a_successful_reply_must_answer_this_request() {
        let ok: Response =
            serde_json::from_str(r#"{"ok":true,"request_id":"7","text":"hi"}"#).unwrap();
        assert!(check_response(&ok, "7").is_ok());
        let err = check_response(&ok, "8").unwrap_err().to_string();
        assert!(err.contains("mismatch"), "{err}");
    }
}
