#!/bin/bash

echo "🚀 Setting up Parakeet CoreML for fast transcription"
echo "=================================================="

# Navigate to parakeet-coreml directory
cd parakeet-coreml

# Download the model if not already present
if [ ! -d "parakeet-tdt-0.6b-v3" ]; then
    echo "📥 Downloading Parakeet CoreML model from HuggingFace..."
    
    # Check if huggingface-cli is installed
    if ! command -v huggingface-cli &> /dev/null; then
        echo "Installing huggingface-hub..."
        pip install -q huggingface-hub
    fi
    
    huggingface-cli download \
        --repo-type model \
        --local-dir parakeet-tdt-0.6b-v3 \
        FluidInference/parakeet-tdt-0.6b-v3-coreml
else
    echo "✅ Model already downloaded"
fi

# Build the Swift CLI
echo "🔨 Building Swift CLI..."
swift build -c release

# Copy the executable
cp .build/release/parakeet-cli ./parakeet-cli
echo "✅ Parakeet CLI built successfully"

# Test the CLI
echo ""
echo "📊 Testing Parakeet CLI..."
echo "Creating test audio..."

# Create a simple test audio file using Python
python3 -c "
import numpy as np
import soundfile as sf

# Generate 1 second of silence
audio = np.zeros(16000, dtype=np.float32)
sf.write('test.wav', audio, 16000)
print('Test audio created')
"

# Test the CLI
if ./parakeet-cli test.wav 2>/dev/null | grep -q "text"; then
    echo "✅ Parakeet CLI is working!"
else
    echo "⚠️  Parakeet CLI test failed - it may not be fully configured yet"
fi

# Clean up
rm -f test.wav

echo ""
echo "🎉 Setup complete!"
echo ""
echo "To use Parakeet in local-dictation:"
echo "  CLI:      uv run local-dictation --engine parakeet"
echo "  Auto:     uv run local-dictation --engine auto"
echo ""
echo "Parakeet is ~5x faster than Whisper but only supports English."