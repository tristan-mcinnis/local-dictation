# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

### Development
```bash
# Install dependencies (using uv)
uv sync

# Run the application
uv run local-dictation

# Run with specific options
uv run local-dictation --model base.en --chord "CMD,SHIFT"

# List available audio devices
uv run local-dictation --print-devices
```

### Package Management
- This project uses `uv` for dependency management
- Dependencies are specified in `pyproject.toml`
- Use `uv add <package>` to add new dependencies
- Use `uv sync` after modifying dependencies

## Performance Optimizations

The implementation includes these performance improvements:
1. **Audio Recording**: Pre-allocated numpy arrays, direct 16kHz recording, cached resampling parameters
2. **Transcription**: Minimal stdout/stderr redirection, optimized output suppression  
3. **System**: Fast debounce (50ms default), responsive ticker rate (5ms)
4. **Model Selection**: Default to medium.en for best balance of speed and accuracy

Typical processing times on Apple Silicon:
- tiny.en: ~100-200ms
- base.en: ~200-400ms
- medium.en: ~800-1200ms (default)
- large-v3-turbo: ~1500-2000ms

## Architecture

This is a push-to-talk dictation tool for macOS that uses Whisper for local speech-to-text transcription. The application runs entirely on-device using Apple Silicon acceleration.

### Core Components

**Entry Flow**
- `src/local_dictation/cli.py`: Main entry point, handles CLI arguments and orchestrates components
- Initializes hotkey listener, audio recorder, and transcriber
- Uses `pynput.keyboard.Controller` to type transcribed text at cursor position

**Hotkey Detection** (`hotkey.py`)
- Global chord detection using `pynput`
- Debounced release mechanism to prevent bounce
- Supports customizable key combinations (CMD+ALT default)
- Thread-safe state management with ticker thread for debounce timing

**Audio Recording** (`audio.py`)
- Real-time audio capture using `sounddevice`
- Ring buffer implementation with max duration limit
- Automatic resampling to 16kHz using polyphase filter from `scipy`
- Optional high-pass filtering for noisy environments
- Device selection by name substring matching

**Transcription** (`transcribe.py`)
- Uses `pywhispercpp` for Whisper model inference
- Leverages Metal acceleration on Apple Silicon
- Suppresses progress output to stdout for clean piping
- Supports multiple output formats: text, lowercase, JSON

### Model Management
- Models are automatically downloaded on first use
- Cached in `~/Library/Application Support/pywhispercpp/models/`
- Default model: `medium.en` (best balance of speed and accuracy for dictation)

### Key Technical Details
- All processing happens locally after initial model download
- Audio is recorded at device's native sample rate, then resampled to 16kHz
- Uses OS-level file descriptor manipulation to suppress whisper.cpp progress messages
- Thread-safe coordination between hotkey detection and audio recording