//! Persistent client for the native Cohere Transcribe MLX helper.
//!
//! The helper is a separate process, so it can die (a Metal out-of-memory, a
//! jetsam kill, a GPU hang) while the daemon carries on. It used to be spawned
//! exactly once and read with a blocking `read_line` that had no timeout, so
//! one crash broke dictation until the app was relaunched, and one hang wedged
//! the worker thread with the pill stuck on "Transcribing" and Escape unable
//! to free it.
//!
//! Two things fix that here:
//!
//! * **A reader thread.** `stdout` is drained on its own thread into a
//!   channel, so every read the worker does is a `recv_timeout` with a budget
//!   scaled to the audio. A hung helper costs one budget, not the session.
//!   The request is written on a short-lived thread under the same budget, so
//!   a helper that stops reading stdin (a full pipe) is a hang too, not a
//!   blocked `write_all`.
//! * **Restart with backoff.** A dead or hung helper is killed, waited for and
//!   respawned. A transport failure (the helper was already gone) retries the
//!   request once on the fresh process; a timeout does not, because the user
//!   should not wait two budgets — the utterance fails with its audio still in
//!   the recovery store. Repeated restart failures raise
//!   [`crate::health::Fault::HelperFailed`], so the menu bar says so instead of
//!   showing an idle icon.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// Floor for one reply, whatever the audio length: model warm-up and the first
/// Metal shader compile happen inside a request.
const MIN_READ_BUDGET: Duration = Duration::from_secs(15);

/// How long the helper gets to load its model and announce itself at boot,
/// when the 1.5 GB checkpoint may still be coming off a cold disk.
const READY_BUDGET: Duration = Duration::from_secs(120);

/// The same wait during a restart. A restart happens inside `transcribe` on the
/// worker thread, so the boot budget would freeze a press for two minutes with
/// the pill stuck on "Transcribing". The weights are in the page cache by then,
/// so a helper that has not announced itself in this long is wedged, not slow:
/// give up, report, and let the next press try again.
const RESTART_READY_BUDGET: Duration = Duration::from_secs(20);

/// How many restarts in a row before the failure is declared to the user.
const RESTARTS_BEFORE_FAULT: u32 = 3;

/// How long to wait for one reply about `samples` of 16 kHz audio. Three times
/// real time, floored, so a long dictation is never cut off by the timeout
/// while a hang is still caught in seconds.
pub fn read_budget(samples: usize) -> Duration {
    let seconds = samples as f64 / 16_000.0;
    let scaled = Duration::from_secs_f64((seconds * 3.0).max(0.0));
    scaled.max(MIN_READ_BUDGET)
}

/// Delay before the nth consecutive restart attempt (0-based). The first is
/// immediate — a crashed helper usually restarts cleanly — and the rest back
/// off so a helper that cannot run does not become a spawn loop.
pub fn restart_backoff(attempt: u32) -> Duration {
    match attempt {
        0 => Duration::ZERO,
        1 => Duration::from_millis(500),
        2 => Duration::from_secs(2),
        _ => Duration::from_secs(5),
    }
}

/// One live helper process and the thread draining its stdout.
struct Helper {
    child: Child,
    /// Shared with the per-request writer thread.
    stdin: Arc<Mutex<BufWriter<ChildStdin>>>,
    /// Lines from the helper. A closed channel means the process has gone.
    replies: Receiver<String>,
}

impl Helper {
    fn spawn(config: &CohereMlxConfig) -> eyre::Result<Self> {
        Self::spawn_with_budget(config, READY_BUDGET)
    }

    fn spawn_with_budget(config: &CohereMlxConfig, ready_budget: Duration) -> eyre::Result<Self> {
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
        let stdin = Arc::new(Mutex::new(BufWriter::new(
            child
                .stdin
                .take()
                .ok_or_else(|| eyre::eyre!("helper stdin unavailable"))?,
        )));
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| eyre::eyre!("helper stdout unavailable"))?;
        let replies = drain_stdout(stdout);
        let mut helper = Self {
            child,
            stdin,
            replies,
        };
        let ready = helper.read_response(ready_budget)?;
        if !ready.ok || ready.event.as_deref() != Some("ready") {
            return Err(response_error(
                "Cohere MLX helper did not become ready",
                &ready,
            ));
        }
        Ok(helper)
    }

    /// Read one reply, or fail within `budget`. The error distinguishes a
    /// process that has gone (retryable on a fresh one) from a hang.
    fn read_response(&mut self, budget: Duration) -> eyre::Result<Response> {
        match self.replies.recv_timeout(budget) {
            Ok(line) => parse_response_line(&line, crate::daemon_log::text_logging_enabled()),
            Err(RecvTimeoutError::Disconnected) => {
                Err(eyre::eyre!("Cohere MLX helper exited unexpectedly"))
            }
            Err(RecvTimeoutError::Timeout) => Err(eyre::eyre!(
                "Cohere MLX helper did not answer within {:.0}s",
                budget.as_secs_f64()
            )),
        }
    }

    /// Write one request, or fail within `budget`. A 30 s chunk is about
    /// 2.5 MB, far past the pipe buffer, so a helper that is alive but not
    /// reading would block `write_all` forever. On timeout the child is killed,
    /// which ends the blocked write with EPIPE.
    fn write_request(&mut self, request: &Request, budget: Duration) -> Result<(), Failure> {
        let mut line = serde_json::to_vec(request).map_err(|e| Failure::Reply(e.into()))?;
        line.push(b'\n');
        let stdin = Arc::clone(&self.stdin);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("cohere-mlx-writer".into())
            .spawn(move || {
                let mut stdin = stdin.lock().unwrap_or_else(|e| e.into_inner());
                let _ = tx.send(stdin.write_all(&line).and_then(|()| stdin.flush()));
            })
            .map_err(|e| Failure::Gone(eyre::eyre!("spawn cohere-mlx writer: {e}")))?;
        match rx.recv_timeout(budget) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(Failure::Gone(eyre::eyre!("write to Cohere MLX helper: {e}"))),
            Err(RecvTimeoutError::Disconnected) => {
                Err(Failure::Gone(eyre::eyre!("Cohere MLX helper writer ended without a result")))
            }
            Err(RecvTimeoutError::Timeout) => {
                let _ = self.child.kill();
                Err(Failure::Hung(eyre::eyre!(
                    "Cohere MLX helper did not take the request within {:.0}s",
                    budget.as_secs_f64()
                )))
            }
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Move stdout onto its own thread. The channel closes on EOF, which is how a
/// dead helper becomes a prompt error instead of a blocked worker.
fn drain_stdout(stdout: ChildStdout) -> Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("cohere-mlx-reader".into())
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            return;
                        }
                    }
                }
            }
        })
        .expect("spawn cohere-mlx reader thread");
    rx
}

pub struct CohereMlxWorker {
    config: CohereMlxConfig,
    /// `None` between a death and a successful restart.
    helper: Option<Helper>,
    sequence: u64,
    /// Consecutive failed restarts, for the backoff and the health fault.
    restarts: u32,
}

impl CohereMlxWorker {
    pub fn start(config: CohereMlxConfig) -> eyre::Result<Self> {
        let helper = Helper::spawn(&config)?;
        Ok(Self {
            config,
            helper: Some(helper),
            sequence: 0,
            restarts: 0,
        })
    }

    pub fn transcribe(&mut self, audio: &[f32]) -> eyre::Result<String> {
        let budget = read_budget(audio.len());
        match self.attempt(audio, budget) {
            Ok(text) => {
                self.restarts = 0;
                crate::health::clear(crate::health::Fault::HelperFailed);
                Ok(text)
            }
            Err(Failure::Hung(e)) => {
                // The process is wedged. Replace it so the NEXT utterance
                // works, but do not make this user wait a second budget.
                eprintln!("[cohere] {e} — restarting the helper");
                self.restart();
                Err(eyre::eyre!("{e}"))
            }
            Err(Failure::Gone(e)) => {
                eprintln!("[cohere] {e} — restarting the helper and retrying once");
                if !self.restart() {
                    return Err(eyre::eyre!("{e}"));
                }
                match self.attempt(audio, budget) {
                    Ok(text) => {
                        self.restarts = 0;
                        crate::health::clear(crate::health::Fault::HelperFailed);
                        Ok(text)
                    }
                    Err(Failure::Hung(e) | Failure::Gone(e) | Failure::Reply(e)) => Err(e),
                }
            }
            // The helper answered and said no. Nothing to restart.
            Err(Failure::Reply(e)) => Err(e),
        }
    }

    /// One request against the current helper.
    fn attempt(&mut self, audio: &[f32], budget: Duration) -> Result<String, Failure> {
        let helper = match self.helper.as_mut() {
            Some(helper) => helper,
            None => return Err(Failure::Gone(eyre::eyre!("Cohere MLX helper is not running"))),
        };
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
        helper.write_request(&request, budget)?;
        let response = match helper.read_response(budget) {
            Ok(response) => response,
            Err(e) => {
                return Err(if e.to_string().contains("did not answer") {
                    Failure::Hung(e)
                } else {
                    Failure::Gone(e)
                })
            }
        };
        if let Err(e) = check_response(&response, &request_id) {
            return Err(classify_reply_failure(&e));
        }
        if let Some(timings) = &response.timings_ms {
            if let Some(total) = timings.get("total") {
                eprintln!("[cohere] helper inference {total:.1} ms");
            }
        }
        Ok(response.text.unwrap_or_default())
    }

    /// Kill whatever is left and spawn a fresh helper. Returns whether the
    /// worker now has a live process.
    fn restart(&mut self) -> bool {
        // Dropping kills and waits, so no zombie is left behind.
        self.helper = None;
        let delay = restart_backoff(self.restarts);
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        match Helper::spawn_with_budget(&self.config, RESTART_READY_BUDGET) {
            Ok(helper) => {
                eprintln!("[cohere] helper restarted");
                self.helper = Some(helper);
                // Not cleared here: one good transcription clears it, so a
                // helper that restarts and dies again stays reported.
                true
            }
            Err(e) => {
                self.restarts = self.restarts.saturating_add(1);
                eprintln!("[cohere] helper restart failed ({}): {e}", self.restarts);
                if self.restarts >= RESTARTS_BEFORE_FAULT {
                    crate::health::raise(
                        crate::health::Fault::HelperFailed,
                        format!("{} restarts failed: {e}", self.restarts),
                    );
                }
                false
            }
        }
    }
}

/// Why one request failed, which decides whether a retry is worth anything.
enum Failure {
    /// The helper answered with an error. Retrying would get the same answer.
    Reply(eyre::Report),
    /// The process is gone or the pipe broke. A fresh helper may well work.
    Gone(eyre::Report),
    /// The process is alive but did not answer in time.
    Hung(eyre::Report),
}

/// Decode one reply line. A line that does not decode may still hold a
/// transcript, and this error is printed to the daemon log, so the line is
/// shown by the log's text rule: its size only, unless `DICTATE_LOG_TEXT=1`.
fn parse_response_line(line: &str, log_text: bool) -> eyre::Result<Response> {
    serde_json::from_str(line).map_err(|e| {
        eyre::eyre!(
            "invalid Cohere MLX response: {e}: {}",
            crate::daemon_log::render_text(log_text, line.trim())
        )
    })
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
            "Cohere MLX {MISMATCH} (sent {request_id}, got {:?})",
            response.request_id
        ));
    }
    Ok(())
}

/// Marker inside the id-mismatch message, so the one string decides both the
/// wording and the recovery.
const MISMATCH: &str = "response id mismatch";

/// Does a rejected reply mean "the helper said no" or "the stream is out of
/// step"?
///
/// An id mismatch is not an answer, it is a desynchronised transport: every
/// later request would read the reply before it, for as long as the process
/// lived. [`Failure::Reply`] deliberately does not restart, so a mismatch
/// classified as one was a permanently dead helper that nothing ever
/// replaced. Treated as [`Failure::Gone`] it costs one respawn and the
/// utterance still lands.
fn classify_reply_failure(error: &eyre::Report) -> Failure {
    if error.to_string().contains(MISMATCH) {
        return Failure::Gone(eyre::eyre!("{error}"));
    }
    Failure::Reply(eyre::eyre!("{error}"))
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

    /// Regression: a restart happens inside `transcribe`, on the worker thread,
    /// so reusing the 120 s boot budget meant one wedged helper froze a key
    /// press for two minutes with the pill stuck on "Transcribing". A restart
    /// reloads weights that are already in the page cache, so its budget has to
    /// be short enough that the user gets an answer, and still comfortably
    /// longer than a warm load.
    #[test]
    fn a_restart_waits_far_less_than_a_cold_boot() {
        assert!(RESTART_READY_BUDGET < READY_BUDGET);
        assert!(RESTART_READY_BUDGET <= Duration::from_secs(30));
        // Longer than one reply's floor, or a slow warm load would be read as
        // a wedge and restarted in a loop.
        assert!(RESTART_READY_BUDGET > MIN_READ_BUDGET);
    }

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
    fn an_undecodable_reply_does_not_put_its_text_in_the_error() {
        // Regression: the raw line (which can hold the transcript) was pasted
        // into the error, and the error is printed to the daemon log.
        let line = r#"{"ok":true,"text":"call Dr. Okafor about the biopsy""#;
        let err = parse_response_line(line, false).unwrap_err().to_string();
        assert!(err.contains("invalid Cohere MLX response"), "{err}");
        assert!(!err.contains("Okafor"), "{err}");
        let debug = parse_response_line(line, true).unwrap_err().to_string();
        assert!(debug.contains("Okafor"), "{debug}");
    }

    #[test]
    fn the_read_budget_scales_with_the_audio_and_never_goes_below_the_floor() {
        // A 200 ms utterance still gets the warm-up floor: the first request
        // after a spawn pays the Metal shader compile.
        assert_eq!(read_budget(3_200), MIN_READ_BUDGET);
        assert_eq!(read_budget(0), MIN_READ_BUDGET);
        // A two-minute hold gets three times real time, not the floor, so a
        // long dictation is never cut off by its own timeout.
        assert_eq!(read_budget(16_000 * 120), Duration::from_secs(360));
        // Monotonic in the audio length.
        assert!(read_budget(16_000 * 60) < read_budget(16_000 * 61));
    }

    #[test]
    fn restarts_back_off_instead_of_becoming_a_spawn_loop() {
        assert_eq!(restart_backoff(0), Duration::ZERO);
        let mut previous = Duration::ZERO;
        for attempt in 1..8 {
            let delay = restart_backoff(attempt);
            assert!(delay >= previous, "attempt {attempt}");
            assert!(delay <= Duration::from_secs(5), "attempt {attempt}");
            previous = delay;
        }
        assert_eq!(restart_backoff(50), Duration::from_secs(5));
    }

    /// Regression for the read that had no timeout: a helper that dies must
    /// surface as an error, promptly, rather than block the worker thread
    /// forever on `read_line`. `/usr/bin/true` exits at once, so its stdout
    /// hits EOF and the reader thread closes the channel.
    #[test]
    fn a_helper_that_exits_at_once_fails_fast_instead_of_blocking() {
        let started = std::time::Instant::now();
        let err = match CohereMlxWorker::start(CohereMlxConfig {
            helper_path: "/usr/bin/true".to_string(),
            model_dir: "/nonexistent".to_string(),
            language: "en".to_string(),
            max_tokens: 64,
        }) {
            Ok(_) => panic!("/usr/bin/true cannot be a working helper"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("exited unexpectedly"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a dead helper must not be waited out: took {:?}",
            started.elapsed()
        );
    }

    /// Regression: the request was written synchronously before any timeout
    /// started, so a helper that announced itself and then stopped reading
    /// stdin blocked the worker forever once the pipe filled.
    #[test]
    fn a_helper_that_stops_reading_is_a_hang_not_a_blocked_write() {
        let dir = std::env::temp_dir().join(format!("ld-cohere-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("deaf-helper.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho '{\"ok\":true,\"event\":\"ready\"}'\nexec sleep 30\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut worker = CohereMlxWorker::start(CohereMlxConfig {
            helper_path: script.to_string_lossy().into_owned(),
            model_dir: "/nonexistent".to_string(),
            language: "en".to_string(),
            max_tokens: 64,
        })
        .expect("the fake helper announces ready");
        let audio = vec![0.25_f32; 16_000 * 30];
        let started = std::time::Instant::now();
        let result = worker.attempt(&audio, Duration::from_millis(300));
        assert!(matches!(result, Err(Failure::Hung(_))), "a full stdin pipe must time out");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "bounded by the budget: took {:?}",
            started.elapsed()
        );
        drop(worker);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_helper_that_cannot_be_launched_is_reported_not_swallowed() {
        let err = match CohereMlxWorker::start(CohereMlxConfig {
            helper_path: "/nonexistent/cohere-mlx-bridge".to_string(),
            model_dir: "/nonexistent".to_string(),
            language: "en".to_string(),
            max_tokens: 64,
        }) {
            Ok(_) => panic!("a missing helper binary cannot start"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("start Cohere MLX helper"), "{err}");
    }

    #[test]
    fn a_successful_reply_must_answer_this_request() {
        let ok: Response =
            serde_json::from_str(r#"{"ok":true,"request_id":"7","text":"hi"}"#).unwrap();
        assert!(check_response(&ok, "7").is_ok());
        let err = check_response(&ok, "8").unwrap_err().to_string();
        assert!(err.contains("mismatch"), "{err}");
    }

    /// Regression: an id mismatch means the reply stream is one line out of
    /// step, so every later request would read the previous request's answer.
    /// It used to be a `Reply` failure, which deliberately never restarts the
    /// helper — a permanently broken transport with no way back short of
    /// relaunching the app. It has to be recoverable.
    #[test]
    fn a_desynchronised_reply_stream_replaces_the_helper_instead_of_wedging() {
        let stale: Response =
            serde_json::from_str(r#"{"ok":true,"request_id":"7","text":"hi"}"#).unwrap();
        let mismatch = check_response(&stale, "8").unwrap_err();
        assert!(
            matches!(classify_reply_failure(&mismatch), Failure::Gone(_)),
            "a mismatch must restart and retry, not be reported as the answer"
        );

        // The helper genuinely answering "no" is still a Reply: restarting
        // would only get the same refusal, slower.
        let refused: Response = serde_json::from_str(
            r#"{"ok":false,"request_id":"8","error_code":"model","error_message":"audio too short"}"#,
        )
        .unwrap();
        let said_no = check_response(&refused, "8").unwrap_err();
        assert!(matches!(classify_reply_failure(&said_no), Failure::Reply(_)));
    }
}
