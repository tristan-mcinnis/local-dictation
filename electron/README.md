# Local Dictation - Electron App

A minimalist macOS menu bar app for local push-to-talk dictation using Whisper.

## Features

- 🎙️ Lives in your menu bar for quick access
- ⚡ Fast local transcription using Whisper
- 🌊 Real-time audio waveform visualization
- 📝 Automatic transcript history
- 🎯 Customizable hotkeys and models
- 🔒 Fully offline after initial model download

## Development

### Prerequisites

1. Node.js 18+ and npm
2. Python 3.10+ with uv package manager
3. macOS (Apple Silicon recommended)

### Setup

```bash
# Install Python dependencies
cd ..
uv sync

# Install Electron dependencies
cd electron
npm install

# Run in development mode
npm run dev
```

### Building for Distribution

```bash
# Build Python standalone executable
cd ..
uv run python build_standalone.py

# Build Electron app
cd electron
npm run dist
```

The built app will be in `electron/dist/Local Dictation.app`

## Usage

1. Launch the app - it appears in your menu bar
2. Click the menu bar icon to access settings and history
3. Hold your configured hotkey (default: Cmd+Alt) to record
4. Release to transcribe and insert text at cursor position

## Architecture

- **Electron Main Process**: Menu bar management, window creation, IPC
- **Python Backend**: Audio recording, Whisper transcription, hotkey detection
- **Renderer Processes**: Settings UI, transcript history, audio visualizer

## File Structure

```
electron/
├── main.js           # Main Electron process
├── settings.html     # Settings window
├── history.html      # Transcript history window
├── visualizer.html   # Audio waveform visualizer
├── assets/          # Icons and resources
└── package.json     # Electron configuration

src/local_dictation/
├── cli_electron.py   # Python backend for Electron
├── audio.py         # Audio recording
├── transcribe.py    # Whisper transcription
└── hotkey.py        # Global hotkey detection
```