# Local Dictation 🎤

A fast, local push-to-talk dictation tool for macOS that automatically inserts transcribed text at your cursor position. Powered by OpenAI's Whisper running entirely on-device via Apple Silicon acceleration.

## 🚀 Optimized Performance

- **800-1200ms** processing time with medium.en model
- **200-400ms** with base.en for ultra-fast response
- Direct 16kHz recording when supported
- Pre-allocated buffers for efficiency

## ✨ Features

- **🔒 100% Local & Private** - All transcription happens on-device using whisper.cpp. No internet required after model download.
- **⚡ Apple Silicon Optimized** - Leverages Metal acceleration for fast transcription on M1/M2/M3 chips
- **🎯 Push-to-Talk** - Hold a customizable hotkey chord (default: ⌘+⌥) to record, release to transcribe
- **✍️ Auto-Insert** - Transcribed text automatically appears at your cursor position in any application
- **🎛️ Customizable** - Configure model, language, hotkey, audio device, and more
- **🔇 Clean Output** - Suppresses progress messages for clean piping to other tools

## 📋 Requirements

- macOS (optimized for Apple Silicon M1/M2/M3)
- Python 3.12+
- Microphone access permission
- Accessibility permission (for global hotkey and text insertion)

## 🚀 Quick Start

### Install with uv (Recommended)

```bash
# Install uv if you haven't already
curl -LsSf https://astral.sh/uv/install.sh | sh

# Clone and install
git clone https://github.com/yourusername/local-dictation.git
cd local-dictation
uv sync

# Run
uv run local-dictation
```

### Install with pip

```bash
# Clone and install
git clone https://github.com/yourusername/local-dictation.git
cd local-dictation
pip install -e .

# Run
local-dictation
```

## 🎮 Usage

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

## 🔧 Configuration Tips

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

## 🏗️ Architecture

```
local-dictation/
├── src/local_dictation/
│   ├── __init__.py
│   ├── __main__.py        # Entry point
│   ├── cli.py             # CLI argument parsing and main loop
│   ├── hotkey.py          # Global hotkey detection
│   ├── audio.py           # Audio recording and resampling
│   └── transcribe.py      # Whisper transcription
├── pyproject.toml         # Package configuration
└── README.md
```

### Key Components

- **Hotkey Listener**: Uses `pynput` for cross-application hotkey detection with debouncing
- **Audio Recorder**: Records via `sounddevice`, resamples with `scipy` polyphase filter
- **Transcriber**: Runs whisper.cpp via `pywhispercpp` with Metal acceleration
- **Auto-typer**: Uses `pynput` to insert text at cursor position

## 🤝 Contributing

Contributions are welcome! Feel free to:

- Report bugs or request features via GitHub Issues
- Submit pull requests with improvements
- Share your custom configurations

## 📜 License

MIT License - see [LICENSE](LICENSE) file for details

## 🙏 Acknowledgments

- [OpenAI Whisper](https://github.com/openai/whisper) for the amazing speech recognition model
- [whisper.cpp](https://github.com/ggerganov/whisper.cpp) for the efficient C++ implementation
- [pywhispercpp](https://github.com/abdeladim-s/pywhispercpp) for Python bindings

## 🔒 Privacy

This tool runs entirely offline after initial model download. No audio or text is sent to any external servers. Your dictation remains completely private on your device.

---

Made with ❤️ for the macOS community. Optimized for Apple Silicon.