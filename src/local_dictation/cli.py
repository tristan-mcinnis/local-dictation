#!/usr/bin/env python3
from __future__ import annotations
import argparse
import os
import sys
from pynput import keyboard
from .hotkey import HotkeyListener, parse_chord
from .audio import VoiceRecorder, list_input_devices
from .transcribe import Transcriber

def env_or(name: str, default: str):
    return os.getenv(name, default)

def build_argparser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser("local-dictation", description="Push-to-talk dictation (macOS, Apple Silicon)")
    p.add_argument("--model", default=env_or("MODEL", "large-v3-turbo-q8_0"))
    p.add_argument("--lang", default=env_or("LANG", "auto"))
    p.add_argument("--chord", default=env_or("CHORD", "CMD,ALT"),
                   help="Chord like 'CMD,ALT' or 'CTRL,SHIFT' or 'F8'")
    p.add_argument("--debounce-ms", type=int, default=int(env_or("DEBOUNCE_MS", "120")))
    p.add_argument("--max-sec", type=float, default=float(env_or("MAX_SEC", "90")))
    p.add_argument("--highpass-hz", type=float, default=float(env_or("HIGHPASS_HZ", "0")))
    p.add_argument("--device", default=env_or("AUDIO_DEVICE", None),
                   help="Substring match of input device name")
    p.add_argument("--output", choices=["text","lower","json"], default=env_or("OUTPUT","text"))
    p.add_argument("--print-devices", action="store_true")
    return p

def main():
    args = build_argparser().parse_args()

    if args.print_devices:
        devs = list_input_devices()
        if not devs:
            print("No input devices.", file=sys.stderr)
            sys.exit(1)
        for d in devs:
            print(f"[{d['index']:02d}] {d['name']}  (default_sr={d['default_samplerate']})")
        return

    chord = parse_chord(args.chord)
    if not chord:
        print(f"Invalid chord: {args.chord}", file=sys.stderr)
        sys.exit(2)

    rec = VoiceRecorder(device_name=args.device,
                        max_sec=args.max_sec,
                        highpass_hz=args.highpass_hz,
                        channels=1)

    tx = Transcriber(model_name=args.model, lang=args.lang)

    print(f"Press and hold chord to record: {args.chord}", file=sys.stderr)

    # Create a keyboard controller for typing
    kbd = keyboard.Controller()
    
    def on_chord(active: bool):
        try:
            if active:
                rec.start()
            else:
                audio = rec.stop()
                if audio is not None and audio.size > 0:
                    text = tx.transcribe(audio, output=args.output)
                    if text:
                        # Type the transcribed text at the cursor position
                        kbd.type(text)
        except Exception as e:
            print(f"Error: {e}", file=sys.stderr)

    listener = HotkeyListener(chord=chord, debounce_ms=args.debounce_ms, on_chord_active=on_chord)
    try:
        listener.start()
        listener.join()
    except KeyboardInterrupt:
        listener.stop()
        print("\nStopped.", file=sys.stderr)