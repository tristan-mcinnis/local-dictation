#!/bin/bash

# Local Dictation Development Script
# This script runs the development version of the menubar app

echo "Starting Local Dictation menubar app in development mode..."

# Check if uv is installed
if ! command -v uv &> /dev/null; then
    echo "Error: uv is not installed. Please install it first:"
    echo "curl -LsSf https://astral.sh/uv/install.sh | sh"
    exit 1
fi

# Sync dependencies
echo "Syncing Python dependencies..."
uv sync

# Navigate to electron directory and install dependencies
echo "Installing Electron dependencies..."
cd electron
if ! command -v npm &> /dev/null; then
    echo "Error: npm is not installed. Please install Node.js first."
    exit 1
fi

npm install

# Run the development version
echo "Starting menubar app..."
npm run dev