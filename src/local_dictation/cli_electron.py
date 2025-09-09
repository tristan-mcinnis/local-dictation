#!/usr/bin/env python3
"""
Electron IPC version of the CLI for integration with the Electron app
"""
from __future__ import annotations
import argparse
import os
import sys
import time
import json
import threading
from pynput import keyboard
from .hotkey import HotkeyListener, parse_chord
from .audio import VoiceRecorder, list_input_devices
from .transcribe import Transcriber
from .type_text import type_text

def env_or(name: str, default: str):
    return os.getenv(name, default)

def build_argparser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser("local-dictation-electron", 
                               description="Electron IPC version of local dictation")
    p.add_argument("--model", default=env_or("MODEL", "medium.en"),
                   help="Whisper model")
    p.add_argument("--lang", default=env_or("LANG", "auto"),
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
    sys.stdout.flush()  # Extra flush to ensure message is sent

def main():
    # Suppress all stderr output from whisper.cpp
    import contextlib
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
    
    def on_chord(active: bool):
        try:
            if active:
                send_message("RECORDING_START")
                rec.start()
            else:
                send_message("RECORDING_STOP")
                
                audio = rec.stop()
                
                if audio is not None and audio.size > 0:
                    text = tx.transcribe(audio, output="text")
                    
                    if text:
                        # Send transcript to Electron for saving
                        send_message("TRANSCRIPT", text)
                        # Minimal delay to ensure window focus is back
                        time.sleep(0.05)
                        
                        # Type the text at cursor position
                        if type_text(text, kbd):
                            send_message("TYPED", "success")
                        else:
                            send_message("TYPE_ERROR", "Failed to type text")
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