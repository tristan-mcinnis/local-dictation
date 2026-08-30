#!/bin/zsh
set -euo pipefail

here="${0:A:h}"
cd "$here"

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
  print -u2 "Cohere MLX bridge requires Apple Silicon macOS."
  exit 2
fi

# MLX Swift's Metal kernels are emitted only by Xcode's package build. A plain
# `swift build` produces an executable that compiles but fails at runtime with
# "Failed to load the default metallib".
xcodebuild -quiet \
  -scheme cohere-mlx-bridge \
  -destination 'platform=macOS,arch=arm64' \
  -configuration Release \
  -derivedDataPath .build/xcode \
  build

product="${here}/.build/xcode/Build/Products/Release"
test -x "${product}/cohere-mlx-bridge"
test -f "${product}/mlx-swift_Cmlx.bundle/Contents/Resources/default.metallib"
print "${product}/cohere-mlx-bridge"
