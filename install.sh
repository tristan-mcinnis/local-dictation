#!/usr/bin/env bash
#
# Local Dictation — one-shot installer.
#
#   ./install.sh
#
# Turns a fresh clone into an installed, login-at-launch menu-bar app:
#   1. checks prerequisites (Rust, Xcode Command Line Tools, cmake)
#   2. downloads the speech + cleanup models (if not already present)
#   3. builds and installs "Local Dictation.app" into /Applications
#   4. registers it as a Login Item and launches it
#
# Safe to re-run: every step is idempotent (skips work already done).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

bold() { printf '\033[1m%s\033[0m\n' "$1"; }
info() { printf '  %s\n' "$1"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$1"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$1"; }
die()  { printf '\033[31m✗ %s\033[0m\n' "$1" >&2; exit 1; }

# Is stdin a terminal? Controls whether we may prompt vs. just proceed.
INTERACTIVE=0
[ -t 0 ] && INTERACTIVE=1

confirm() { # confirm "question" -> 0 if yes
  [ "$INTERACTIVE" -eq 0 ] && return 0   # non-interactive: assume yes
  local reply
  read -r -p "  → $1 [Y/n] " reply || true
  [[ -z "$reply" || "$reply" =~ ^[Yy] ]]
}

echo
bold "Local Dictation installer"
echo

# ── 0. platform sanity ───────────────────────────────────────────────────────
[ "$(uname -s)" = "Darwin" ] || die "This app is macOS-only."
if [ "$(uname -m)" != "arm64" ]; then
  warn "This is built for Apple Silicon (M-series). Intel Macs are untested."
fi

# ── 1. prerequisites ─────────────────────────────────────────────────────────
bold "1/4  Checking prerequisites"

# Xcode Command Line Tools (provides the compiler/linker Rust needs).
if ! xcode-select -p >/dev/null 2>&1; then
  warn "Xcode Command Line Tools are missing."
  if confirm "Trigger their installer now?"; then
    xcode-select --install || true
    die "Finish the Command Line Tools install in the popup, then re-run ./install.sh"
  else
    die "Install them with:  xcode-select --install"
  fi
fi
ok "Xcode Command Line Tools"

# Rust toolchain (cargo).
if ! command -v cargo >/dev/null 2>&1; then
  # rustup may be installed but not on PATH for this shell yet.
  [ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
fi
if ! command -v cargo >/dev/null 2>&1; then
  warn "Rust (cargo) is not installed."
  if confirm "Install Rust now via rustup?"; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
  else
    die "Install Rust from https://rustup.rs then re-run ./install.sh"
  fi
fi
ok "Rust $(cargo --version | awk '{print $2}')"

# cmake (needed to build the llama.cpp / Metal cleanup engine).
if ! command -v cmake >/dev/null 2>&1; then
  warn "cmake is not installed."
  if command -v brew >/dev/null 2>&1; then
    if confirm "Install cmake via Homebrew?"; then
      brew install cmake
    else
      die "Install cmake (brew install cmake) then re-run ./install.sh"
    fi
  else
    die "cmake is required. Install Homebrew (https://brew.sh) then: brew install cmake"
  fi
fi
ok "cmake $(cmake --version | head -1 | awk '{print $3}')"

# ── 2. models ────────────────────────────────────────────────────────────────
echo
bold "2/4  Models (Parakeet speech-to-text + Gemma cleanup, ~1.4 GB)"
PARAKEET="models/dictation/parakeet-tdt-v3-int8/encoder-model.int8.onnx"
GEMMA="models/llm/gemma-3-1b-it/gemma-3-1b-it-Q4_K_M.gguf"
if [ -s "$PARAKEET" ] && [ -s "$GEMMA" ]; then
  ok "models already downloaded"
else
  info "downloading (one time)…"
  ./scripts/download-models.sh
fi

# ── 3+4. build, install, login item, launch ──────────────────────────────────
echo
bold "3/4  Building and installing the app"
info "this compiles the release build the first time — a few minutes is normal."
./scripts/build-app.sh --install

echo
bold "4/4  Almost done — one thing only you can do"
cat <<'EOT'
  macOS will ask for two permissions the first time the app runs. Grant both:
    • Microphone     — so it can hear you
    • Accessibility  — so it can type the text where your cursor is
  (System Settings → Privacy & Security. If you don't see a prompt, add
   "Local Dictation" under Accessibility manually.)

  Then you're set. How to use it:
    • Hold  Right Option , speak, release — text appears at your cursor.
    • Hands-free: hold Right Option + tap Space, release both, keep talking,
      tap Right Option again to stop.
    • Look for the 🎤 in your menu bar. It launches automatically at login.
EOT
echo
ok "Installed: /Applications/Local Dictation.app"
