# Local Dictation & Assistant

A fast, local push-to-talk dictation tool for macOS that automatically inserts transcribed text at your cursor position. Powered by OpenAI's Whisper running entirely on-device via Apple Silicon acceleration.

## Optimized Performance

- **800-1200ms** processing time with medium.en model
- **200-400ms** with base.en for ultra-fast response
- Direct 16kHz recording when supported
- Pre-allocated buffers for efficiency

## Features

- **100% Local & Private** - All transcription happens on-device using whisper.cpp. No internet required after model download.
- **Apple Silicon Optimized** - Leverages Metal acceleration for fast transcription on M1/M2/M3 chips
- **Push-to-Talk** - Hold a customizable hotkey chord (default: ⌘+⌥) to record, release to transcribe
- **Auto-Insert** - Transcribed text automatically appears at your cursor position in any application
- **Assistant Mode** (NEW) - Process spoken commands to manipulate selected text using on-device MLX models
- **Customizable** - Configure model, language, hotkey, audio device, and more
- **Clean Output** - Suppresses progress messages for clean piping to other tools

## Requirements

- macOS (optimized for Apple Silicon M1/M2/M3)
- Python 3.12+
- Microphone access permission
- Accessibility permission (for global hotkey and text insertion)

## Quick Start

### Development Setup (Recommended for Latest Features)

**For the menubar app with GUI:**
```bash
# Install uv if you haven't already
curl -LsSf https://astral.sh/uv/install.sh | sh

# Clone the repository
git clone https://github.com/tristan-mcinnis/local-dictation.git
cd local-dictation

# Run the development script
./dev-run.sh
```

The development script will:
1. Sync Python dependencies with uv
2. Install Electron dependencies
3. Launch the menubar app in development mode

### CLI-Only Installation

**With uv (Recommended):**
```bash
# Install uv if you haven't already
curl -LsSf https://astral.sh/uv/install.sh | sh

# Clone and install
git clone https://github.com/tristan-mcinnis/local-dictation.git
cd local-dictation
uv sync

# Run CLI version
uv run local-dictation
```

**With pip:**
```bash
# Clone and install
git clone https://github.com/tristan-mcinnis/local-dictation.git
cd local-dictation
pip install -e .

# Run CLI version
local-dictation
```

## Usage

### Basic Usage

1. Start the application:
   ```bash
   uv run local-dictation
   
   # With benchmark mode to see timing
   uv run local-dictation --benchmark
   ```

2. Hold `⌘ + ⌥` (Cmd + Option) to start recording

3. Speak clearly into your microphone

4. Release the keys to stop recording and transcribe

5. The transcribed text automatically appears at your cursor position!

### Command-Line Options

```bash
# Use a different Whisper model (smaller = faster, larger = more accurate)
uv run local-dictation --model base.en

# Change the hotkey chord
uv run local-dictation --chord "CTRL,SHIFT"

# List available audio devices
uv run local-dictation --print-devices

# Use a specific microphone
uv run local-dictation --device "MacBook Pro Microphone"

# Output in lowercase
uv run local-dictation --output lower

# Set maximum recording duration (seconds)
uv run local-dictation --max-sec 60

# Apply high-pass filter for noisy environments
uv run local-dictation --highpass-hz 100
```

### Available Whisper Models

Models are downloaded automatically on first use:

- `tiny`, `tiny.en` - Fastest, least accurate (~39 MB)
- `base`, `base.en` - Good balance (~74 MB)
- `small`, `small.en` - Better accuracy (~244 MB)
- `medium`, `medium.en` - Even better (~769 MB)
- `medium`, `medium.en` - Great accuracy, fast (default, ~769 MB)
- `large-v3-turbo-q8_0` - Best accuracy, slower (~834 MB)

Models with `.en` suffix are English-only and typically faster.

### Environment Variables

All command-line options can be set via environment variables:

```bash
export MODEL=medium.en      # Default: medium.en
export LANG=en              # Default: en
export CHORD="CMD,SHIFT"    # Default: CMD,ALT
export DEBOUNCE_MS=50       # Default: 50
export MAX_SEC=90           # Default: 90
export HIGHPASS_HZ=0        # Default: 0 (disabled)
export OUTPUT=text          # Default: text
export AUDIO_DEVICE="External Microphone"

uv run local-dictation  # Uses environment settings
```

## Assistant Mode (NEW)

Assistant mode enables voice-controlled text manipulation using on-device MLX language models. When enabled, you can select text and speak commands to process it.

### Enabling Assistant Mode

**CLI:**
```bash
# Enable assistant mode with default model
uv run local-dictation --assistant-mode

# Use a specific model
uv run local-dictation --assistant-mode --assistant-model "mlx-community/Llama-3.2-1B-Instruct-4bit"
```

**Electron App:**
Enable in Settings → Assistant Mode

### Supported Commands

When text is selected, speak commands like:
- "Rewrite this in formal language"
- "Make this more concise"
- "Explain this"
- "Summarize this"
- "Translate this to Spanish"
- "Fix this"

### Available Models

- `mlx-community/Llama-3.2-3B-Instruct-4bit` - Default, best for grammar correction
- `mlx-community/Llama-3.2-1B-Instruct-4bit` - Smaller, faster
- `mlx-community/Qwen3-1.7B-4bit` - Alternative small model
- `mlx-community/Qwen3-3B-4bit` - Alternative medium model  
- `mlx-community/SmolLM2-1.7B-Instruct-4bit` - Alternative small model

### Pre-downloading Models

To avoid connection issues, pre-download models:
```bash
uv run python download_assistant_model.py --model "mlx-community/Llama-3.2-3B-Instruct-4bit"
```

Models are cached locally after first download.

## Configuration Tips

### Hotkey Chords

You can use various key combinations:

- `CMD,ALT` - Command + Option (default)
- `CTRL,SHIFT` - Control + Shift
- `F8` - Single function key
- `CMD,SHIFT,A` - Three-key combination

### Performance Optimization

- **For maximum speed**: Use `tiny.en` or `base.en` models
- **For best balance**: Use `medium.en` (default)
- **For maximum accuracy**: Use `large-v3-turbo-q8_0`
- **For noisy environments**: Enable high-pass filter with `--highpass-hz 100`

### Performance Features

This optimized implementation includes:
- **Pre-allocated buffers**: Eliminates dynamic memory allocation
- **Direct 16kHz recording**: Skips resampling when device supports it
- **Cached parameters**: Pre-computes resampling and filter coefficients
- **Minimal I/O overhead**: Optimized stdout/stderr handling
- **Fast debounce**: 50ms default for quick response
- **Model flexibility**: Choose speed vs accuracy tradeoff

### Troubleshooting

**"No speech detected"**
- Speak closer to the microphone
- Check your audio input device with `--print-devices`
- Increase recording time by holding the chord longer

**Permission errors**
- Grant Accessibility permission in System Settings → Privacy & Security → Accessibility
- Grant Microphone permission when prompted

**Model download issues**
- Models are cached in `~/Library/Application Support/pywhispercpp/models/`
- Delete corrupted models and restart to re-download

## Architecture

```
local-dictation/
├── src/local_dictation/
│   ├── __init__.py
│   ├── __main__.py        # Entry point
│   ├── cli.py             # CLI argument parsing and main loop
│   ├── cli_electron.py    # Electron IPC version
│   ├── hotkey.py          # Global hotkey detection
│   ├── audio.py           # Audio recording and resampling
│   ├── transcribe.py      # Whisper transcription
│   ├── assistant.py       # MLX assistant mode
│   └── type_text.py       # Text insertion utilities
├── electron/              # Electron app files
│   ├── main.js           # Main process
│   ├── settings.html     # Settings window
│   ├── history.html      # Transcript history
│   └── debug.html        # Debug logs
├── pyproject.toml        # Package configuration
└── README.md
```

### Key Components

- **Hotkey Listener**: Uses `pynput` for cross-application hotkey detection with debouncing
- **Audio Recorder**: Records via `sounddevice`, resamples with `scipy` polyphase filter
- **Transcriber**: Runs whisper.cpp via `pywhispercpp` with Metal acceleration
- **Auto-typer**: Uses `pynput` to insert text at cursor position
- **Assistant**: MLX language models for text processing commands (grammar fixing, rewriting, etc.)

## Contributing

Contributions are welcome! Feel free to:

- Report bugs or request features via GitHub Issues
- Submit pull requests with improvements
- Share your custom configurations

## License

MIT License - see [LICENSE](LICENSE) file for details

## Acknowledgments

- [OpenAI Whisper](https://github.com/openai/whisper) for the amazing speech recognition model
- [whisper.cpp](https://github.com/ggerganov/whisper.cpp) for the efficient C++ implementation
- [pywhispercpp](https://github.com/abdeladim-s/pywhispercpp) for Python bindings

## Privacy

This tool runs entirely offline after initial model download. No audio or text is sent to any external servers. Your dictation remains completely private on your device.

---

Made for the macOS community. Optimized for Apple Silicon.