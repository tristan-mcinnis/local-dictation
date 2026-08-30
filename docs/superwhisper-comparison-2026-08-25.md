# Superwhisper comparison — 2026-08-25

Fixture: `testdata/dictation_sample.wav`, mono 16 kHz, 8.35 seconds.
Machine: Apple M5. Both applications used Cohere Transcribe Q4 and S1-mini.

Both applications produced the identical raw transcript:

> So, yeah, we should build the Rust loop and then like compile it on Mac OS,
> right? Um, the Parakeet ONNX runtime is the core dependency.

## Results

| Application/run | Cohere / transcription | Cleanup | Combined model time |
|---|---:|---:|---:|
| Local Dictation 1 | 200 ms | 9.6 ms | 209 ms |
| Local Dictation 2 | 196 ms | 10.1 ms | 206 ms |
| Superwhisper file run 1 | 320 ms | 408 ms | 728 ms |
| Superwhisper file run 2 | 777 ms | 296 ms | 1,073 ms |

Superwhisper timings came directly from the new `recording` rows and matching
`meta.json` files (`processingTime` plus `languageModelProcessingTime`). Local
Dictation timings came from the native helper response and cleanup benchmark.

## Cleaned text

Superwhisper:

> So, yeah, we should build the Rust loop and then compile it on Mac OS, right?
> The Parakeet ONNX runtime is the core dependency.

Local Dictation:

> Yeah, we should build the Rust loop and then like compile it on Mac OS, right?
> The Parakeet ONNX runtime is the core dependency.

Both preserve the meaning and remove `Um`; Superwhisper removes `like`, while
Local Dictation removes the opening `So`.

## Interpretation

Local Dictation is roughly 3.5–5.2× faster on this identical medium-length file
(about 4.3× using the two-run means). Its cleanup number is not a pure
like-for-like S1 decode: the production safety path may accept deterministic
cleanup when S1 returns suspicious output. Superwhisper completed its full S1
rewrite. The output on this fixture is nevertheless comparable.

The test does not measure focus capture, paste visibility, or clipboard restore.
Local Dictation's direct AX path is about 5 ms; Electron clipboard fallback is
materially slower and should be benchmarked separately in the actual target app.
