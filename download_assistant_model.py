#!/usr/bin/env python3
"""
Utility script to pre-download assistant models for offline use.
This helps avoid connection issues during first run.
"""

import sys
import argparse
from mlx_lm import load

def main():
    parser = argparse.ArgumentParser(description="Download MLX models for assistant mode")
    parser.add_argument("--model", default="mlx-community/Qwen3-1.7B-4bit",
                        help="Model to download (default: mlx-community/Qwen3-1.7B-4bit)")
    args = parser.parse_args()
    
    print(f"Downloading model: {args.model}")
    print("This may take a few minutes on first download...")
    print("The model will be cached for future use.\n")
    
    try:
        model, tokenizer = load(args.model)
        print(f"\n✅ Model '{args.model}' downloaded successfully!")
        print("You can now use assistant mode with this model.")
    except KeyboardInterrupt:
        print("\n\n⚠️  Download interrupted. Please run again to complete.")
        sys.exit(1)
    except Exception as e:
        print(f"\n❌ Error downloading model: {e}")
        print("\nTroubleshooting:")
        print("1. Check your internet connection")
        print("2. Try again - downloads can be resumed")
        print("3. Try a different model:")
        print("   - mlx-community/Llama-3.2-1B-Instruct-4bit (smaller)")
        print("   - mlx-community/SmolLM2-1.7B-Instruct-4bit")
        sys.exit(1)

if __name__ == "__main__":
    main()