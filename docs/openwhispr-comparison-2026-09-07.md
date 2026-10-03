# OpenWhispr comparison — 2026-09-07

Sibling to `superwhisper-comparison-2026-08-25.md`. Compares
[OpenWhispr](https://github.com/OpenWhispr/openwhispr) against `local-dictation`
(dictation slice) and `rti` (meeting-transcription slice).

## Verdict

OpenWhispr is breadth, our two apps are depth. It overlaps both but beats
neither at the one job each app exists to do. Worth mining for ideas, not
adopting.

- OpenWhispr: Electron 41 + React 19, macOS/Windows/Linux, ~330 MB macOS dmg.
  Cloud-first by default (OpenWhispr Cloud + Neon Postgres). Local is opt-in.
- Backed by a real company (Gizmo Labs), paid tiers, SSO/SCIM, hosted MCP.
  7.6k stars, multiple releases/week, and matching regressions (Windows
  sign-in broken in 1.9.1, three transcription paths broken, loopback silence).
- Our apps: native (Rust/MLX, Swift), software-of-one, no accounts, no quotas.

## Dictation slice — local-dictation

Wins for us:
- Latency ~200-270ms fully on-device. OpenWhispr publishes no latency numbers
  and routes through spawned helper processes.
- Zero network, no telemetry code path, no account. OpenWhispr defaults to
  cloud and sends custom-dictionary terms with every cloud call.
- 5ms native AX injection + fail-closed clipboard fallback.
- Guarded S1-mini cleanup that skips generation on clean speech.

Wins for them:
- Cross-platform + real installer + notarized (no Gatekeeper dance).
- Engine choice: 6 Whisper sizes + Parakeet/Nemotron + cloud + BYOK.
- Translate-dictation hotkey.

### Decisions

- **Silero VAD: already settled, not needed.** We built and tested it
  (`examples/silero_seg.rs`, `examples/vad_stream_lab.rs`, deleted in the
  fixed-stack pivot commit c6b80d7). Conclusion recorded in `src/vad.rs`: a
  learned VAD (Silero) was not worth its cost on clean audio; the pure-DSP
  energy-based pause segmenter tracks oracle boundaries and wins on cost. In
  push-to-talk dictation the key-hold is the VAD; VAD only matters for
  sentence-splitting during streaming cleanup, where cheap DSP is enough.
  Silero earns its keep only in continuous/noisy audio — that's the diarization
  domain, i.e. `rti`, not dictation.
- **Translate-dictation hotkey: skip.** It requires cloud translation (or a
  local MT model), both against the zero-network design. Not needed.

## Meeting slice — rti

Wins for us:
- Dual-channel capture (mic + system audio as separate legs) via CoreAudio
  process tap, no Screen Recording permission, SCK fallback.
- Meeting-grade resilience: watchdogs, idle park/resume (dodges Soniox 408s),
  default-output-device rebuild.
- Provider-agnostic STT/LLM layers.
- Offline transcript-upgrade lane on retained WAV legs.
- Screen-privacy deny-list + `sharingType = .none`.
- Local archiving into a private notes store, drained by a LaunchAgent.

Wins for them:
- Local speaker diarization + cross-meeting voice fingerprinting
  (sherpa-onnx pyannote segmentation 3.0 + 3D-Speaker CAM++ + Silero VAD),
  up to 8 speakers, live.
- Meeting auto-detection + calendar "join & take notes".
- Local STT option (Whisper/Parakeet/Nemotron) — rti is cloud-only STT today.

## The two real gaps worth stealing (both rti, not dictation)

1. **Local speaker diarization** — the pyannote + CAM++ + Silero combo on
   sherpa-onnx. This is where Silero actually matters.
2. **Local STT for rti** — a Parakeet/Nemotron or Whisper local fallback so
   meeting transcription can run offline.

Both fit the existing local-models daemon; no Electron, no duplicate runtime.

## What we did not miss (dictation)

- Nothing real. Engine choice was a deliberate fixed-stack pivot (Cohere Q4).
- Cross-platform and notarization are outside software-of-one scope.
- Notes/semantic-search, team sharing, API/MCP belong to a separate notes
  layer, not the dictation app.
