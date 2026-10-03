# Local Dictation @VERSION@

Hold a key, talk, and on-device text lands at your cursor. Local Dictation is a free push-to-talk dictation app for Apple Silicon Macs. Speech goes to Cohere Transcribe (4-bit, MLX), then a small cleanup model, "S1-mini" by "Superwhisper", tidies it only when the speech was messy. Nothing leaves your Mac while you dictate.

## What is new

@WHATS_NEW@

## Requirements

- macOS 14 (Sonoma) or later.
- An Apple Silicon Mac (M1 or later). Intel Macs are not supported.
- About 2 GB of free disk for the two models, and a network connection for the one-time model download.

## Download

`@DMG@` (@SIZE@, arm64). This build contains the app only. The two models are not inside it. See "Get the models" below.

@FIRST_OPEN@

## Get the models

The app starts without models and its Setup window tells you what is missing.

1. The cleanup model, "S1-mini" (462 MB): click Get model in the Setup window.
2. The speech model, Cohere Transcribe Q4 (1.5 GB): the app cannot fetch it yet. Run this once in Terminal:

```bash
"/Applications/Local Dictation.app/Contents/Resources/download-models.sh"
```

   It downloads both models from Hugging Face into `~/Library/Application Support/Local Dictation/models`, checks every file against a pinned SHA-256, and needs no Xcode. Then quit and reopen Local Dictation.

## Updates

This free build does not update itself. To update, download the new DMG from Releases and replace the app in Applications.

## Verify the download

SHA-256 of `@DMG@`:

```
@SHA256@
```

Check it with `shasum -a 256 -c SHA256SUMS` in the folder that holds both files.

Built from commit `@COMMIT@`.
