#!/usr/bin/env python3
"""
CoreML Parakeet transcription via Swift CLI
Fast native transcription for Apple Silicon
"""
from __future__ import annotations
import os
import sys
import json
import subprocess
import tempfile
import numpy as np
import soundfile as sf
from typing import Optional, Dict, Any
from pathlib import Path

class ParakeetCoreMLTranscriber:
    """
    Parakeet transcriber using CoreML via Swift CLI
    - Native Apple Silicon performance
    - ~5x faster than real-time
    - No Python dependency issues
    """
    
    def __init__(self, custom_words: Optional[Dict[str, str]] = None):
        self.custom_words = custom_words or {}
        
        # Temporarily disable Parakeet until Swift CLI is properly implemented
        # The CoreML model structure is more complex than initially anticipated
        raise RuntimeError("Parakeet CoreML is not yet fully implemented. Please use --engine whisper for now.")
        
        self.cli_path = self._find_cli()
        
        if not self.cli_path:
            raise RuntimeError("Parakeet CLI not found. Run parakeet-coreml/download_model.sh to set up.")
    
    def _find_cli(self) -> Optional[str]:
        """Find the Parakeet CLI executable"""
        possible_paths = [
            "./parakeet-coreml/parakeet-cli",
            "./parakeet-cli",
            Path.home() / "parakeet-cli",
            Path(__file__).parent.parent.parent / "parakeet-coreml" / "parakeet-cli"
        ]
        
        for path in possible_paths:
            path = Path(path)
            if path.exists() and path.is_file():
                return str(path.absolute())
        
        return None
    
    def apply_custom_words(self, text: str) -> str:
        """Apply custom word replacements"""
        if not self.custom_words:
            return text
        
        for old_word, new_word in self.custom_words.items():
            # Case-insensitive replacement while preserving original case
            import re
            pattern = re.compile(re.escape(old_word), re.IGNORECASE)
            
            def replace_func(match):
                original = match.group(0)
                if original.isupper():
                    return new_word.upper()
                elif original[0].isupper():
                    return new_word.capitalize()
                else:
                    return new_word.lower()
            
            text = pattern.sub(replace_func, text)
        
        return text
    
    def transcribe(self, audio: np.ndarray, sample_rate: int = 16000) -> Optional[str]:
        """
        Transcribe audio using CoreML Parakeet
        
        Args:
            audio: Audio samples as numpy array (float32, mono)
            sample_rate: Sample rate of the audio
            
        Returns:
            Transcribed text or None if no speech detected
        """
        if len(audio) == 0:
            return None
        
        # Save audio to temporary file
        with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as tmp_file:
            tmp_path = tmp_file.name
            sf.write(tmp_path, audio, sample_rate)
        
        try:
            # Call Swift CLI with shorter timeout to avoid hanging
            print(f"🔄 Calling Parakeet CLI at: {self.cli_path}", file=sys.stderr)
            result = subprocess.run(
                [self.cli_path, tmp_path],
                capture_output=True,
                text=True,
                timeout=5  # Reduced timeout to avoid hanging
            )
            
            if result.returncode == 0:
                # Parse JSON output
                output = json.loads(result.stdout)
                text = output.get("text", "").strip()
                
                if text:
                    # Apply custom words
                    text = self.apply_custom_words(text)
                    
                    # Log performance info
                    processing_time = output.get("processingTime", 0)
                    audio_length = output.get("audioLength", 0)
                    if processing_time > 0 and audio_length > 0:
                        rtf = processing_time / audio_length
                        print(f"🚀 Parakeet: {processing_time*1000:.0f}ms for {audio_length:.1f}s audio (RTF: {rtf:.2f})", file=sys.stderr)
                    
                    return text
                else:
                    print("(no speech detected)", file=sys.stderr)
                    return None
            else:
                # Parse error from stderr
                try:
                    error_json = json.loads(result.stderr)
                    error_msg = error_json.get("error", "Unknown error")
                except:
                    error_msg = result.stderr
                
                print(f"Parakeet error: {error_msg}", file=sys.stderr)
                return None
                
        except subprocess.TimeoutExpired:
            print("Parakeet timeout - falling back to Whisper", file=sys.stderr)
            return None
        except Exception as e:
            print(f"Parakeet error: {e}", file=sys.stderr)
            return None
        finally:
            # Clean up temp file
            if os.path.exists(tmp_path):
                os.unlink(tmp_path)
    
    def is_available(self) -> bool:
        """Check if Parakeet CLI is available"""
        return self.cli_path is not None