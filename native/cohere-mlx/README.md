# Cohere MLX bridge

This directory is an isolated native helper for the Rust daemon. It keeps one
`CohereTranscribeModel` resident in a Swift process and communicates over
newline-delimited JSON on stdin/stdout. There is no Python runtime and no
model work happens until a `transcribe` request arrives.

The implementation uses the upstream `Blaizzy/mlx-audio-swift` package's
`MLXAudioSTT.CohereTranscribeModel`, which supports loading the local MLX
Safetensors checkpoint with `fromDirectory`. The model directory must contain
`config.json`, one or more `.safetensors` files, and the tokenizer files. The
ONNX files currently used by the Rust `transcribe-rs` backend are not accepted
by this helper; convert/download the MLX checkpoint separately.

## Build

```sh
./build.sh
```

The first build downloads Swift package dependencies and uses Xcode so MLX's
required `mlx-swift_Cmlx.bundle/default.metallib` is generated. The resulting
arm64 helper is kept alive for the daemon's lifetime.

```sh
DICTATE_COHERE_MLX_MODEL_DIR=/path/to/cohere-transcribe-mlx \
  .build/xcode/Build/Products/Release/cohere-mlx-bridge
```

`--model-dir`, `--language`, and `--max-tokens` are also available. Model load
diagnostics go to stderr; stdout is protocol-only.

## Rust integration contract (v1)

Each request and response is one UTF-8 JSON object followed by `\n`.

Request:

```json
{"op":"transcribe","request_id":"123","sample_rate":16000,"channels":1,"language":"en","max_tokens":256,"audio_f32_base64":"<little-endian f32 PCM>"}
```

`audio_f32_base64` is mono, finite, little-endian IEEE-754 f32 PCM at exactly
16,000 Hz. The Rust side owns resampling/downmixing. A request with invalid
audio receives an error response and does not terminate the helper.

Startup response:

```json
{"ok":true,"event":"ready","model":"cohere-transcribe-mlx","sample_rate":16000}
```

Success response:

```json
{"ok":true,"request_id":"123","text":"...","model":"...","sample_rate":16000,"samples":32000,"timings_ms":{"inference":123.4,"total":124.1}}
```

Supported control requests are `health` and `shutdown`. Error responses have
`ok:false`, `error_code` (`protocol`, `model`, `internal`, or `usage`), and an
`error_message`; `request_id` is echoed when it could be decoded.

The Rust worker should:

1. Spawn the process with piped stdin/stdout/stderr.
2. Read exactly one `event=ready` line before admitting dictation work.
3. Serialize transcribe requests because the model object is intentionally
   single-flight and keeps peak memory stable.
4. Read one response line per request and match `request_id`.
5. Kill/restart the helper after EOF, a non-zero exit, or a malformed response.
6. Treat `timings_ms.total` as helper-side request latency and record the Rust
   encode/write/read/paste timings separately.

For a future lower-copy integration, the same runner can be wrapped in a Unix
socket or a C ABI, but the persistent stdio protocol is the safer first
integration boundary and already eliminates model reload latency.

## Tests

```sh
./test.sh
```

The current tests are dependency-light protocol tests. A model-backed smoke
test should be run on the target Mac once the MLX checkpoint is available:

```sh
printf '%s\n' '{"op":"health","request_id":"smoke"}' | \
  DICTATE_COHERE_MLX_MODEL_DIR=/path/to/model \
  .build/arm64-apple-macosx/release/cohere-mlx-bridge
```
