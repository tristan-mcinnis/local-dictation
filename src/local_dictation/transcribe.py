#!/usr/bin/env python3
from __future__ import annotations
import sys
import os
import tempfile
import orjson
from typing import Literal
from pywhispercpp.model import Model

OutputMode = Literal["text", "lower", "json"]

class Transcriber:
    def __init__(self, model_name: str, lang: str = "auto"):
        print(f"Loading Whisper model: {model_name}", file=sys.stderr)
        self.model = Model(model_name, language=lang, redirect_whispercpp_logs_to=sys.stderr)

    def transcribe(self, audio_f32_mono_16k, output: OutputMode = "text"):
        # Capture stdout at OS level during transcription
        old_stdout = os.dup(1)  # Save current stdout
        
        with tempfile.TemporaryFile(mode='w+') as temp_stdout:
            # Redirect stdout to temp file
            os.dup2(temp_stdout.fileno(), 1)
            
            try:
                # Run transcription (Progress messages go to redirected stdout)
                segments = self.model.transcribe(audio_f32_mono_16k)
            finally:
                # Restore original stdout
                os.dup2(old_stdout, 1)
                os.close(old_stdout)
            
            # Read what was captured but don't print it
            # (this prevents Progress messages from appearing)
        
        # Extract text from segments
        text = " ".join(s.text for s in segments).strip()

        # Process output based on mode
        if output == "lower":
            text = text.lower() if text else ""
        elif output == "json":
            if text:
                payload = {
                    "text": text,
                    "segments": [{"t0": s.t0, "t1": s.t1, "text": s.text} for s in segments],
                }
                text = orjson.dumps(payload).decode("utf-8")
            else:
                text = ""
        
        if not text:
            print("(no speech detected)", file=sys.stderr)
            return None
        
        # Also print to stdout for debugging/piping if needed
        print(text, flush=True)
        
        # Return the text for automatic typing
        return text