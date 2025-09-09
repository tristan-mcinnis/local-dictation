#!/usr/bin/env python3
"""
Enhanced Electron IPC version with real-time audio monitoring
"""
from __future__ import annotations
import argparse
import os
import sys
import time
import json
import threading
import numpy as np
from pynput import keyboard
from .hotkey import HotkeyListener, parse_chord
from .audio import VoiceRecorder, list_input_devices
from .transcribe import Transcriber

def env_or(name: str, default: str):
    return os.getenv(name, default)

def build_argparser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser("local-dictation-electron", 
                               description="Electron IPC version of local dictation")
    p.add_argument("--model", default=env_or("MODEL", "medium.en"),
                   help="Whisper model")
    p.add_argument("--lang", default=env_or("LANG", "en"),
                   help="Language code")
    p.add_argument("--chord", default=env_or("CHORD", "CMD,ALT"),
                   help="Chord like 'CMD,ALT' or 'CTRL,SHIFT'")
    p.add_argument("--debounce-ms", type=int, default=int(env_or("DEBOUNCE_MS", "50")))
    p.add_argument("--max-sec", type=float, default=float(env_or("MAX_SEC", "90")))
    p.add_argument("--highpass-hz", type=float, default=float(env_or("HIGHPASS_HZ", "0")))
    p.add_argument("--device", default=env_or("AUDIO_DEVICE", None))
    return p

def send_message(msg_type: str, data: str = ""):
    """Send IPC message to Electron"""
    message = f"{msg_type}:{data}" if data else msg_type
    print(message, file=sys.stdout, flush=True)
    sys.stdout.flush()

def main():
    # Suppress all stderr output from whisper.cpp
    sys.stderr = open(os.devnull, 'w')
    
    args = build_argparser().parse_args()

    chord = parse_chord(args.chord)
    if not chord:
        send_message("ERROR", f"Invalid chord: {args.chord}")
        sys.exit(2)

    rec = VoiceRecorder(device_name=args.device,
                        max_sec=args.max_sec,
                        highpass_hz=args.highpass_hz,
                        channels=1)

    tx = Transcriber(model_name=args.model, lang=args.lang)

    # Send ready signal
    send_message("READY", json.dumps({
        "model": args.model,
        "chord": args.chord,
        "device_rate": rec.samplerate,
        "needs_resample": rec.needs_resample
    }))

    # Create a keyboard controller for typing
    kbd = keyboard.Controller()
    
    # Audio level monitoring
    monitoring = False
    
    def monitor_audio_levels():
        """Send real-time audio levels to Electron"""
        while monitoring:
            if rec.recording and rec.buffer:
                # Get recent audio samples
                recent_samples = rec.buffer[-1024:]  # Last 1024 samples
                if len(recent_samples) > 0:
                    # Calculate RMS level
                    rms = np.sqrt(np.mean(recent_samples**2))
                    # Normalize to 0-1 range (assuming 16-bit audio range)
                    level = min(1.0, rms * 10)  # Scale factor for visualization
                    send_message("AUDIO_LEVEL", str(level))
            time.sleep(0.05)  # Update 20 times per second
    
    monitor_thread = None
    
    def on_chord(active: bool):
        nonlocal monitoring, monitor_thread
        try:
            if active:
                send_message("RECORDING_START")
                rec.start()
                # Start audio monitoring
                monitoring = True
                monitor_thread = threading.Thread(target=monitor_audio_levels, daemon=True)
                monitor_thread.start()
            else:
                send_message("RECORDING_STOP")
                monitoring = False
                
                audio = rec.stop()
                
                if audio is not None and audio.size > 0:
                    text = tx.transcribe(audio, output="text")
                    
                    if text:
                        # Send transcript to Electron for saving
                        send_message("TRANSCRIPT", text)
                        # Small delay to ensure window focus is back
                        time.sleep(0.1)
                        # Type the text at cursor position
                        kbd.type(text)
        except Exception as e:
            send_message("ERROR", str(e))

    listener = HotkeyListener(chord=chord, debounce_ms=args.debounce_ms, on_chord_active=on_chord)
    
    # Also listen for stdin commands from Electron
    import select
    
    def stdin_listener():
        while True:
            try:
                if sys.stdin in select.select([sys.stdin], [], [], 0)[0]:
                    line = sys.stdin.readline().strip()
                    if line == "START":
                        # Manual start from Electron menu
                        on_chord(True)
                        time.sleep(0.1)  # Brief delay
                        on_chord(False)
                    elif line == "QUIT":
                        listener.stop()
                        sys.exit(0)
            except:
                pass
            time.sleep(0.1)
    
    stdin_thread = threading.Thread(target=stdin_listener, daemon=True)
    stdin_thread.start()
    
    try:
        listener.start()
        listener.join()
    except KeyboardInterrupt:
        listener.stop()
        send_message("STOPPED")

if __name__ == "__main__":
    main()