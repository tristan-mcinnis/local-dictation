# Fixed-stack verification — 2026-08-24

## Build and tests

- `cargo fmt --check`: pass
- `cargo check --features full`: pass, no warnings
- `cargo test --features full`: 205 library + 2 S1 corpus + 2 integration tests passed
- `native/cohere-mlx/test.sh`: protocol tests passed
- `native/cohere-mlx/build.sh`: Xcode release build passed; metallib generated
- `scripts/build-app.sh`: signed development app bundle built successfully
- Bundled helper health check: passed using the app's colocated `mlx.metallib`
- Removed production dependency graph: no Parakeet, Moonshine, Whisper,
  `transcribe-rs`, ONNX Runtime, or `ort`

## Measured fixture latency

Machine: Apple M5. Fixture: the earlier human-voice `testdata/dictation_sample.wav`, 8.35 seconds
(replaced by a synthetic clip on 2026-10-29).

| Stage | Warm latency |
|---|---:|
| Cohere Q4 MLX | 198–220 ms |
| Safeguarded S1-mini cleanup on fixture | 9.6 ms |
| Combined inference | 209.3 ms |

Cohere's first request after process start was about 229 ms in the final run;
model load was 433 ms and is paid once at daemon boot.

## S1-mini quality gate

The forced-model 12-case corpus currently passes 4/12 strict whole-case gates,
with check-level results:

- content preservation: 87%
- dictionary spellings: 71%
- filler cleanup: 83%
- no-answer: 82%
- no-hallucination: 94%
- punctuation: 100%
- latency: p50 115 ms, p90 382 ms

This is not strong enough to trust unconditionally. Production therefore:

1. skips S1-mini for already-clean dictation;
2. applies exact personal corrections before and after generation;
3. detects empty, ballooned, repeated-token and chat-marker output;
4. detects dropped clauses;
5. falls back to deterministic mechanics-preserving cleanup.

On the real fixture the guarded result correctly removed the filler `Um` and
preserved both clauses. The eval remains checked in so prompt/runtime changes
cannot silently weaken these safeguards.

## Manual checks still required

- Grant Microphone and Accessibility after installing the rebuilt ad-hoc app.
- Exercise hover expansion, cancel/settings hit targets and reduced motion in a
  live AppKit session.
- Listen to the original synthesized cue set at the user's normal output volume.
- Measure Slack/Chrome/VS Code clipboard visibility separately from clipboard
  restoration; Electron paste remains the next likely latency target.
