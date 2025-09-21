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
- **Assistant Mode** (NEW) - Process spoken commands using on-device MLX models or OpenAI's latest GPT-5 family
- **MCP Tooling** - Optionally connect GPT-5 to local Model Context Protocol servers for tool-enabled workflows
- **TextEdit Previews** - Optionally open assistant responses in a macOS TextEdit window while keeping them on the clipboard
- **Customizable** - Configure model, language, hotkey, audio device, and more
- **Wake Words** - Hands-free activation with configurable phrases and VAD auto-finish on silence
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

# Use GPT-5-mini for assistant mode and preview responses in TextEdit
export OPENAI_API_KEY=sk-...
uv run local-dictation --assistant-mode --assistant-provider openai \
  --assistant-openai-model gpt-5-mini --assistant-result-action show_textedit
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
export ASSISTANT_PROVIDER=openai           # Use GPT-5
export ASSISTANT_OPENAI_MODEL=gpt-5-mini   # GPT-5 model to use
export ASSISTANT_RESULT_ACTION=show_textedit
export ASSISTANT_COPY_RESULT=false         # Keep clipboard unchanged after replace
export ASSISTANT_OPENAI_KEY=sk-...         # Or set OPENAI_API_KEY directly

uv run local-dictation  # Uses environment settings
```

### Building whisper.cpp with Metal/CoreML

For best performance on Apple Silicon, build `whisper.cpp` with Metal and CoreML enabled:

```bash
cmake -B build -S . \
  -DGGML_METAL=ON \
  -DWHISPER_COREML=ON \
  -DGGML_ACCELERATE=ON
cmake --build build -j
```

At runtime, enable the accelerated paths:

```c
struct whisper_full_params p = whisper_full_default_params(WHISPER_SAMPLING_GREEDY);
p.use_gpu = true;          // Metal backend
p.coreml_encode = true;    // CoreML encoder
p.no_timestamps = true;    // no token timestamps
p.temperature = 0.0f;
```

Greedy sampling or a small beam size (1-2) gives the lowest latency for dictation.

## Assistant Mode (NEW)

Assistant mode enables voice-controlled text manipulation. You can keep everything local with MLX language models, or switch to OpenAI's GPT-5 family for richer rewrites, explanations, and research assistance.

### Enabling Assistant Mode

**CLI:**
```bash
# Local MLX assistant (default)
uv run local-dictation --assistant-mode

# Use a specific MLX model
uv run local-dictation --assistant-mode --assistant-model "mlx-community/Llama-3.2-1B-Instruct-4bit"
```

**Electron App:**
Enable in Settings → Assistant Mode

### Choosing an Assistant Provider

By default the assistant uses MLX models that run entirely on your Mac. To route commands through OpenAI's GPT-5 Responses API instead:

```bash
export OPENAI_API_KEY=sk-...
uv run local-dictation --assistant-mode --assistant-provider openai --assistant-openai-model gpt-5-mini
```

- `gpt-5` – maximum reasoning depth and context.
- `gpt-5-mini` – balanced latency and quality (default when `provider=openai`).
- `gpt-5-nano` – fastest responses for lightweight edits.

You can also set `ASSISTANT_PROVIDER`, `ASSISTANT_OPENAI_MODEL`, or edit `~/.config/local-dictation/config.yaml` to persist your preference. Use `--assistant-openai-key-env` if your API key lives in a custom environment variable.

### Result Actions

Control how the response is delivered after a command:

- `auto` *(default)* – rewrites/fixes replace the selection, while explain/summarize/research commands open a temporary TextEdit window and copy the result to the clipboard.
- `replace_selection` – always replace the highlighted text.
- `copy_to_clipboard` – keep the original selection untouched and copy the response.
- `show_textedit` – open every response in TextEdit (also copied unless you pass `--assistant-no-copy-result`).

Adjust behaviour with `--assistant-result-action`, `--assistant-copy-result`, or matching keys in the config file.

### Tooling with the Model Context Protocol

When you select the OpenAI provider you can allow GPT-5 to call local tools via the [Model Context Protocol (MCP)](https://github.com/modelcontextprotocol). Enable this with `--assistant-use-mcp` and provide one or more servers via `--assistant-mcp-server label="python -m my_server"` or by pointing to a JSON/YAML manifest with `--assistant-mcp-config`.

Each MCP server runs locally and communicates over stdio. The assistant automatically lists tools and forwards results back to GPT-5, so replies can incorporate live data from your Mac. You can also set `--assistant-mcp-timeout` to control how long we wait for servers to start.

Example configuration snippet:

```yaml
assistant:
  provider: openai
  use_mcp: true
  mcp_startup_timeout: 15.0
  mcp_servers:
    - label: search
      command: python
      args:
        - -m
        - my_company.search_mcp
    - label: notes
      command: /usr/local/bin/notes-mcp
      env:
        NOTES_ROOT: /Users/me/Documents/Notes
```

With the CLI you can add ad-hoc servers on launch:

```bash
uv run local-dictation --assistant-mode --assistant-provider openai \
  --assistant-use-mcp \
  --assistant-mcp-server search="python -m my.search.server" \
  --assistant-mcp-server calendar="/usr/local/bin/calendar-mcp"
```

### Supported Commands

When text is selected, speak commands like:
- "Rewrite this in formal language"
- "Make this more concise"
- "Explain this"
- "Summarize this"
- "Translate this to Spanish"
- "Fix this"
- "Research this" (provides background context and follow-up ideas)

### Available Models

**Local MLX models**

- `mlx-community/Llama-3.2-3B-Instruct-4bit` – Default, great for grammar correction
- `mlx-community/Llama-3.2-1B-Instruct-4bit` – Smaller, faster
- `mlx-community/Qwen3-1.7B-4bit` – Alternative small model
- `mlx-community/Qwen3-3B-4bit` – Alternative medium model
- `mlx-community/SmolLM2-1.7B-Instruct-4bit` – Alternative small model

**OpenAI GPT-5 models**

- `gpt-5` – Highest quality for deep rewrites and analysis
- `gpt-5-mini` – Balanced quality and latency
- `gpt-5-nano` – Fastest option for quick edits

### Pre-downloading MLX Models

To avoid connection issues, pre-download MLX models locally:
```bash
uv run python download_assistant_model.py --model "mlx-community/Llama-3.2-3B-Instruct-4bit"
```

Models are cached locally after first download.

## Configuration Tips

### Assistant Configuration

The config file at `~/.config/local-dictation/config.yaml` now includes an `assistant` section. Example:

```yaml
assistant:
  enabled: false
  provider: mlx
  model: mlx-community/Llama-3.2-3B-Instruct-4bit
  openai_model: gpt-5-mini
  openai_api_key_env: OPENAI_API_KEY
  result_action: auto
  copy_result_to_clipboard: true
  temperature: 0.2
  max_output_tokens: 900
  use_mcp: false
  mcp_startup_timeout: 15.0
  mcp_servers: []
```

Set `provider` to `openai` and update `openai_model` (gpt-5 / gpt-5-mini / gpt-5-nano) to make GPT-5 the default. You can also set `openai_api_key` directly, or point `openai_api_key_env` to a custom environment variable name.

### Hotkey Chords

You can use various key combinations:

- `CMD,ALT` - Command + Option (default)
- `CTRL,SHIFT` - Control + Shift
- `F8` - Single function key
- `CMD,SHIFT,A` - Three-key combination

### Hands-Free Wake Words

Prefer a voice-first workflow? Supply one or more wake phrases to trigger recording automatically:

```bash
uv run local-dictation --wake-words "hey dictation,start writing"
```

The detector listens in the background and starts recording when a wake phrase is spoken. Silence is tracked with VAD so recording stops automatically once you finish speaking. Fine-tune behaviour with:

- `--wake-word-window-sec` – audio window to analyze (default 2.5s)
- `--wake-word-gap-sec` – minimum seconds between detections (default 2s)
- `--wake-word-match-threshold` – fuzzy match ratio for spoken text (default 0.78)
- `--wake-word-vad-threshold` – speech probability required to capture a segment (default 0.55)

Persist phrases in the config file:

```yaml
audio:
  model: medium.en
  language: en
  max_seconds: 90
  wake_words:
    - "hey dictation"
    - "start writing"
```

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
- **Assistant**: MLX or OpenAI GPT-5 models for text processing commands (rewriting, research, formatting)

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

When you stick with the default MLX provider, the assistant runs entirely on-device and no audio or text leaves your Mac. If you opt into `--assistant-provider openai`, selected text and prompts are sent to OpenAI's API so you can use GPT-5 models—review OpenAI's privacy policy before enabling that mode.

---

Made for the macOS community. Optimized for Apple Silicon.