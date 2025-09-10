#!/bin/bash

# Download Parakeet CoreML model
echo "Downloading Parakeet CoreML model..."

# Check if huggingface-cli is installed
if ! command -v huggingface-cli &> /dev/null; then
    echo "Installing huggingface-hub..."
    pip install -q huggingface-hub
fi

# Download the model
echo "Downloading from HuggingFace..."
huggingface-cli download \
    --repo-type model \
    --local-dir parakeet-tdt-0.6b-v3 \
    FluidInference/parakeet-tdt-0.6b-v3-coreml

echo "Model downloaded to parakeet-tdt-0.6b-v3/"

# Build the Swift CLI
echo "Building Swift CLI..."
swift build -c release

# Copy the executable to a convenient location
cp .build/release/parakeet-cli ./parakeet-cli

echo "✅ Setup complete!"
echo "You can now use: ./parakeet-cli <audio_file>"