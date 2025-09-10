# Parakeet CoreML Integration

Fast, native transcription using CoreML on Apple Silicon.

## Features

- **~5x faster than real-time** transcription
- **Native Apple Silicon performance** via CoreML
- **No Python dependencies** - runs as compiled Swift
- **Automatic fallback** to Whisper if unavailable
- **English-only** (for now)

## Setup

Run the setup script to download the model and build the CLI:

```bash
./setup_parakeet.sh
```

This will:
1. Download the Parakeet CoreML model from HuggingFace
2. Build the Swift CLI tool
3. Test the installation

## Usage

### CLI
```bash
# Use Parakeet explicitly
uv run local-dictation --engine parakeet

# Auto-select best engine (Parakeet if available)
uv run local-dictation --engine auto

# Force Whisper
uv run local-dictation --engine whisper
```

### Electron App
Select the engine in Settings → Transcription Engine

## Performance Comparison

| Engine | Speed | Accuracy | Languages | Memory |
|--------|-------|----------|-----------|---------|
| Parakeet | ~5x real-time | Good | English | ~600MB |
| Whisper (medium) | ~1x real-time | Excellent | 99+ | ~1.5GB |

## Architecture

```
Audio → Swift CLI → CoreML → JSON → Python → Text
```

The Swift CLI (`parakeet-cli`) handles:
- Audio file loading
- CoreML model inference  
- JSON output

The Python wrapper (`transcribe_parakeet.py`) handles:
- Temporary file management
- Custom word replacements
- Integration with the main app

## Troubleshooting

If Parakeet isn't working:

1. **Check Swift version**: Requires Swift 5.9+ and macOS 14+
   ```bash
   swift --version
   ```

2. **Verify model download**:
   ```bash
   ls parakeet-coreml/parakeet-tdt-0.6b-v3/
   ```

3. **Test CLI directly**:
   ```bash
   ./parakeet-coreml/parakeet-cli test.wav
   ```

4. **Check for CoreML support**:
   Must be running on Apple Silicon (M1/M2/M3)

## Fallback Behavior

When using `--engine auto`:
1. Tries Parakeet first
2. Falls back to Whisper if:
   - Parakeet CLI not found
   - CoreML model not available
   - Running on non-Apple Silicon
   - Transcription fails

## Future Improvements

- [ ] Multi-language Parakeet models
- [ ] Streaming transcription
- [ ] Direct Python bindings (no subprocess)
- [ ] Intel Mac support via ONNX