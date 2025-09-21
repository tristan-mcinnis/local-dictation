#!/usr/bin/env python3
"""
Optimized local dictation with improved performance
"""
from __future__ import annotations
import argparse
import os
import sys
import time
import json
import re
import threading
from pathlib import Path
from typing import List
from pynput import keyboard
from .hotkey import HotkeyListener, parse_chord
from .audio import VoiceRecorder, list_input_devices
from .vad import SileroVAD
from .transcribe import Transcriber
from .transcribe_unified import UnifiedTranscriber
from .assistant import Assistant
from .config import get_config_path, load_config
from .dictation_config import DictationConfig
from .endpoint_manager import EndpointManager
from .wake_word import WakeWordDetector, build_wake_word_config
from .mcp_client import parse_mcp_server_strings

def env_or(name: str, default: str | None):
    return os.getenv(name, default)


def env_flag(name: str) -> bool | None:
    value = os.getenv(name)
    if value is None:
        return None
    return value.lower() in {"1", "true", "yes", "on"}


def env_float(name: str) -> float | None:
    value = os.getenv(name)
    if value is None:
        return None
    try:
        return float(value)
    except ValueError:
        return None


def env_int(name: str) -> int | None:
    value = os.getenv(name)
    if value is None:
        return None
    try:
        return int(value)
    except ValueError:
        return None

def build_argparser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser("local-dictation", 
                               description="Fast push-to-talk dictation (macOS, Apple Silicon)")
    p.add_argument("--model", default=env_or("MODEL", "medium.en"),
                   help="Whisper model (tiny.en, base.en, small.en, medium.en, large-v3-turbo-q8_0)")
    p.add_argument("--lang", default=env_or("LANG", "en"),
                   help="Language code (en, auto, etc.)")
    p.add_argument("--chord", default=env_or("CHORD", "CMD,ALT"),
                   help="Chord like 'CMD,ALT' or 'CTRL,SHIFT' or 'F8'")
    p.add_argument("--debounce-ms", type=int, default=int(env_or("DEBOUNCE_MS", "50")),
                   help="Key release debounce in milliseconds")
    p.add_argument("--max-sec", type=float, default=float(env_or("MAX_SEC", "90")))
    p.add_argument("--highpass-hz", type=float, default=float(env_or("HIGHPASS_HZ", "0")))
    p.add_argument("--device", default=env_or("AUDIO_DEVICE", None),
                   help="Substring match of input device name")
    p.add_argument("--output", choices=["text","lower","json"], default=env_or("OUTPUT","text"))
    p.add_argument("--print-devices", action="store_true")
    p.add_argument("--benchmark", action="store_true", 
                   help="Show performance timing for each transcription")
    p.add_argument("--assistant-mode", action="store_true",
                   help="Enable assistant mode for processing commands on selected text")
    p.add_argument("--assistant-provider", choices=["mlx", "openai"],
                   default=env_or("ASSISTANT_PROVIDER", None),
                   help="Assistant backend to use: 'mlx' for local models or 'openai' for GPT-5")
    p.add_argument("--assistant-model", default=env_or("ASSISTANT_MODEL", None),
                   help="MLX model to use for assistant mode (when provider=mlx)")
    p.add_argument("--assistant-openai-model", default=env_or("ASSISTANT_OPENAI_MODEL", None),
                   help="OpenAI model to use (gpt-5, gpt-5-mini, gpt-5-nano) when provider=openai")
    p.add_argument("--assistant-openai-key", default=env_or("ASSISTANT_OPENAI_KEY", None),
                   help="OpenAI API key (optional if set via environment variable)")
    p.add_argument("--assistant-openai-key-env", default=env_or("ASSISTANT_OPENAI_KEY_ENV", None),
                   help="Environment variable name containing the OpenAI API key")
    p.add_argument("--assistant-openai-organization", default=env_or("ASSISTANT_OPENAI_ORG", None),
                   help="OpenAI organization ID (optional)")
    p.add_argument("--assistant-openai-base-url", default=env_or("ASSISTANT_OPENAI_BASE_URL", None),
                   help="Custom base URL for OpenAI-compatible endpoints (optional)")
    p.add_argument("--assistant-result-action",
                   choices=["auto", "replace_selection", "copy_to_clipboard", "show_textedit"],
                   default=env_or("ASSISTANT_RESULT_ACTION", None),
                   help="How to deliver assistant results (default: auto)")
    p.add_argument("--assistant-temperature", type=float, default=None,
                   help="Sampling temperature for assistant responses")
    p.add_argument("--assistant-max-output-tokens", type=int, default=None,
                   help="Maximum number of tokens for assistant responses")
    p.add_argument("--assistant-copy-result", dest="assistant_copy_result", action="store_true",
                   help="Always copy assistant output to the clipboard")
    p.add_argument("--assistant-no-copy-result", dest="assistant_copy_result", action="store_false",
                   help="Do not keep assistant output on the clipboard after replacing text")
    p.set_defaults(assistant_copy_result=None)
    p.add_argument("--assistant-use-mcp", dest="assistant_use_mcp", action="store_true",
                   help="Allow the assistant to use Model Context Protocol tool servers")
    p.add_argument("--assistant-no-mcp", dest="assistant_use_mcp", action="store_false",
                   help="Disable MCP tool usage even if configured")
    p.set_defaults(assistant_use_mcp=None)
    p.add_argument("--assistant-mcp-server", action="append",
                   help="Add an MCP server definition as label=command with optional args")
    p.add_argument("--assistant-mcp-config", default=env_or("ASSISTANT_MCP_CONFIG", None),
                   help="Path to a JSON or YAML file describing MCP servers")
    p.add_argument("--assistant-mcp-timeout", type=float, default=None,
                   help="Seconds to wait for MCP servers to start")
    p.add_argument("--use-vad", action="store_true",
                   help="Enable VAD (Voice Activity Detection) to filter silence")
    p.add_argument("--wake-words", default=env_or("WAKE_WORDS", None),
                   help="Comma-separated wake phrases for hands-free activation")
    p.add_argument("--wake-word-window-sec", type=float, default=float(env_or("WAKE_WORD_WINDOW_SEC", "2.5")),
                   help="Seconds of audio to analyze for each wake-word attempt")
    p.add_argument("--wake-word-gap-sec", type=float, default=float(env_or("WAKE_WORD_GAP_SEC", "2.0")),
                   help="Minimum seconds between wake detections")
    p.add_argument("--wake-word-match-threshold", type=float,
                   default=float(env_or("WAKE_WORD_MATCH_THRESHOLD", "0.78")),
                   help="Fuzzy match ratio required to trigger wake detection")
    p.add_argument("--wake-word-vad-threshold", type=float,
                   default=float(env_or("WAKE_WORD_VAD_THRESHOLD", "0.55")),
                   help="Silero speech probability threshold for wake detection")
    p.add_argument("--idle-timeout", type=int, default=int(env_or("IDLE_TIMEOUT", "60")),
                   help="Seconds before unloading idle model (0=never)")
    p.add_argument("--custom-words", type=str, default=env_or("CUSTOM_WORDS", None),
                   help="JSON file with custom word replacements")
    p.add_argument("--engine", choices=["whisper", "auto"], default=env_or("ENGINE", "whisper"),
                   help="Transcription engine")
    return p

def main():
    args = build_argparser().parse_args()
    
    # Load config (creates default if doesn't exist)
    config_path = get_config_path()
    if not config_path.exists():
        print(f"🔧 Creating config file at: {config_path}", file=sys.stderr)
        config = load_config()  # This will create default config
        print(f"📝 Please edit the config to set your email sign-off", file=sys.stderr)
    else:
        config = load_config()

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

    # Load custom words if provided
    custom_words = {}
    if args.custom_words:
        try:
            with open(args.custom_words, 'r') as f:
                custom_words = json.load(f)
            print(f"📖 Loaded {len(custom_words)} custom word replacements", file=sys.stderr)
        except Exception as e:
            print(f"⚠️ Failed to load custom words: {e}", file=sys.stderr)

    # Force English language for .en models
    lang = 'en' if args.model.endswith('.en') else args.lang
    
    # Use unified transcriber for engine selection
    tx = UnifiedTranscriber(
        engine=args.engine,
        model_name=args.model,
        lang=lang,
        idle_timeout_seconds=args.idle_timeout,
        custom_words=custom_words
    )

    transcribe_lock = threading.Lock()

    wake_words: List[str] = []
    if args.wake_words:
        wake_words = [word.strip() for word in re.split(r'[\n]+|,', args.wake_words) if word.strip()]
    if not wake_words:
        config_wake = config.get('audio', {}).get('wake_words')
        if isinstance(config_wake, str):
            wake_words = [word.strip() for word in re.split(r'[\n]+|,', config_wake) if word.strip()]
        elif isinstance(config_wake, list):
            wake_words = [str(word).strip() for word in config_wake if isinstance(word, (str, bytes)) and str(word).strip()]

    wake_words_enabled = bool(wake_words)
    enable_vad_for_recording = args.use_vad or wake_words_enabled
    vad_threshold = args.wake_word_vad_threshold if wake_words_enabled else 0.5
    shared_vad = None
    endpoint_manager = None

    if enable_vad_for_recording:
        try:
            shared_vad = SileroVAD(threshold=vad_threshold, min_speech_duration_ms=200, min_silence_duration_ms=150)
            if wake_words_enabled:
                cfg = DictationConfig(
                    use_push_to_talk=False,
                    use_auto_stop_vad=True,
                    vad_prob_threshold=vad_threshold,
                    vad_hangover_ms=400,
                    vad_debounce_ms=250,
                )
            else:
                cfg = DictationConfig(
                    use_push_to_talk=True,
                    use_auto_stop_vad=False,
                    vad_prob_threshold=vad_threshold,
                )
            endpoint_manager = EndpointManager(cfg, vad=shared_vad)
        except Exception as exc:
            print(f"⚠️ Failed to initialize shared VAD: {exc}", file=sys.stderr)
            shared_vad = None
            endpoint_manager = None
            enable_vad_for_recording = args.use_vad

    rec = VoiceRecorder(
        device_name=args.device,
        max_sec=args.max_sec,
        highpass_hz=args.highpass_hz,
        channels=1,
        use_vad=enable_vad_for_recording,
        vad_instance=shared_vad,
        endpoint_manager=endpoint_manager,
    )

    assistant_config = config.get('assistant', {})

    # Initialize assistant if in assistant mode
    assistant = None
    if args.assistant_mode:
        print(f"🤖 Initializing Assistant Mode...", file=sys.stderr)

        assistant_provider = (args.assistant_provider or assistant_config.get('provider') or 'mlx').lower()
        openai_model = (
            args.assistant_openai_model
            or assistant_config.get('openai_model')
            or ('gpt-5-mini' if assistant_provider == 'openai' else None)
        )

        if assistant_provider == 'openai':
            model_name = openai_model or 'gpt-5-mini'
        else:
            model_name = (
                args.assistant_model
                or assistant_config.get('model')
                or 'mlx-community/Llama-3.2-3B-Instruct-4bit'
            )

        result_action = args.assistant_result_action or assistant_config.get('result_action', 'auto')

        copy_result = assistant_config.get('copy_result_to_clipboard', True)
        copy_env = env_flag('ASSISTANT_COPY_RESULT')
        if copy_env is not None:
            copy_result = copy_env
        if args.assistant_copy_result is not None:
            copy_result = args.assistant_copy_result

        temperature = assistant_config.get('temperature', 0.2)
        temp_env = env_float('ASSISTANT_TEMPERATURE')
        if temp_env is not None:
            temperature = temp_env
        if args.assistant_temperature is not None:
            temperature = args.assistant_temperature

        max_tokens = assistant_config.get('max_output_tokens', 900)
        max_env = env_int('ASSISTANT_MAX_OUTPUT_TOKENS')
        if max_env is not None:
            max_tokens = max_env
        if args.assistant_max_output_tokens is not None:
            max_tokens = args.assistant_max_output_tokens

        openai_api_key = args.assistant_openai_key or assistant_config.get('openai_api_key')
        openai_key_env = args.assistant_openai_key_env or assistant_config.get('openai_api_key_env', 'OPENAI_API_KEY')
        openai_org = args.assistant_openai_organization or assistant_config.get('openai_organization')
        openai_base_url = args.assistant_openai_base_url or assistant_config.get('openai_base_url')

        use_mcp = assistant_config.get('use_mcp', False)
        if args.assistant_use_mcp is not None:
            use_mcp = args.assistant_use_mcp

        mcp_servers_config = assistant_config.get('mcp_servers', []) or []
        extra_servers: List[object] = []
        if args.assistant_mcp_config:
            config_file = Path(args.assistant_mcp_config).expanduser()
            if config_file.exists():
                try:
                    with open(config_file, 'r') as fh:
                        try:
                            loaded = json.load(fh)
                        except json.JSONDecodeError:
                            fh.seek(0)
                            try:
                                import yaml  # type: ignore
                            except ImportError:
                                yaml = None  # type: ignore
                            if yaml is None:
                                raise
                            loaded = yaml.safe_load(fh)
                    if isinstance(loaded, dict):
                        loaded = loaded.get('servers') or loaded.get('mcp_servers') or []
                    if isinstance(loaded, list):
                        extra_servers.extend(loaded)
                except Exception as exc:
                    print(f"⚠️ Failed to load MCP server config: {exc}", file=sys.stderr)
            else:
                print(f"⚠️ MCP server config not found: {config_file}", file=sys.stderr)

        if args.assistant_mcp_server:
            try:
                extra_servers.extend(parse_mcp_server_strings(args.assistant_mcp_server))
            except ValueError as exc:
                print(f"⚠️ Invalid MCP server specification: {exc}", file=sys.stderr)

        mcp_servers = list(mcp_servers_config) + extra_servers
        mcp_timeout = assistant_config.get('mcp_startup_timeout', 15.0)
        if args.assistant_mcp_timeout is not None:
            mcp_timeout = args.assistant_mcp_timeout
        use_mcp = use_mcp and bool(mcp_servers)

        assistant_kwargs = {
            'model_name': model_name,
            'provider': assistant_provider,
            'openai_model': openai_model,
            'openai_api_key': openai_api_key,
            'openai_api_key_env': openai_key_env,
            'openai_organization': openai_org,
            'openai_base_url': openai_base_url,
            'result_action': result_action,
            'copy_result_to_clipboard': copy_result,
            'temperature': temperature,
            'max_output_tokens': max_tokens,
            'system_prompt': assistant_config.get('system_prompt'),
            'more_info_prompt': assistant_config.get('more_info_prompt'),
            'use_mcp': use_mcp,
            'mcp_servers': mcp_servers,
            'mcp_startup_timeout': mcp_timeout,
        }

        assistant = Assistant(**assistant_kwargs)
        assistant.enable()
        if assistant.enabled:
            provider_label = "OpenAI" if assistant.provider == "openai" else "MLX"
            print(f"🤖 Assistant Mode: ON ({provider_label}: {assistant.model_name})", file=sys.stderr)
            print(f"   Result delivery: {assistant.result_action}", file=sys.stderr)
            print(
                "   Commands: 'rewrite this...', 'explain this', 'summarize this', 'translate to...', 'fix this', 'research this'",
                file=sys.stderr
            )
            if assistant.provider == "openai":
                print("   Using OpenAI Responses API (gpt-5 / gpt-5-mini / gpt-5-nano)", file=sys.stderr)
            if assistant.use_mcp:
                print(f"   MCP tooling: enabled ({len(assistant.mcp_servers)} server(s))", file=sys.stderr)
        else:
            print(
                "⚠️  Assistant Mode failed to initialize. Running in dictation-only mode.",
                file=sys.stderr,
            )
            if assistant_provider == 'openai':
                print(
                    f"   Verify that the OpenAI API key is available via {openai_key_env}.",
                    file=sys.stderr,
                )
            assistant = None

    print(f"🎤 Local Dictation", file=sys.stderr)
    print(f"Engine: {tx.get_active_engine()}", file=sys.stderr)
    if tx.get_active_engine() == "whisper":
        print(f"Model: {args.model}", file=sys.stderr)
    print(f"Press and hold {args.chord} to record", file=sys.stderr)
    print(f"Debounce: {args.debounce_ms}ms", file=sys.stderr)
    if enable_vad_for_recording:
        if wake_words_enabled:
            print(f"🔇 VAD: Enabled (auto stop on silence)", file=sys.stderr)
        else:
            print(f"🔇 VAD: Enabled (silence filtering)", file=sys.stderr)
    if wake_words_enabled:
        print(f"🔔 Wake words: {', '.join(wake_words)}", file=sys.stderr)
        print("   Say the wake phrase or use the hotkey to begin recording.", file=sys.stderr)
    if args.idle_timeout > 0 and tx.get_active_engine() == "whisper":
        print(f"💤 Model unload after: {args.idle_timeout}s idle", file=sys.stderr)
    
    if rec.needs_resample:
        print(f"⚠️  Device rate: {rec.samplerate}Hz (will resample to 16kHz)", file=sys.stderr)
        if wake_words_enabled:
            print("   Wake-word VAD auto-stop works best with native 16kHz capture.", file=sys.stderr)
    else:
        print(f"✅ Direct 16kHz recording (no resampling needed)", file=sys.stderr)

    # Create a keyboard controller for typing
    kbd = keyboard.Controller()
    
    # Performance tracking
    timings = []
    recording_start = None
    stop_lock = threading.Lock()
    wake_detector = None

    def begin_recording(trigger: str = "hotkey"):
        nonlocal recording_start
        with stop_lock:
            if rec._active:
                return
            recording_start = time.perf_counter()
            if wake_detector:
                wake_detector.pause()
            rec.start()
            if trigger == "wake":
                print("🎙️ Wake phrase detected", file=sys.stderr)

    def complete_recording(source: str = "manual"):
        nonlocal recording_start
        with stop_lock:
            if not rec._active:
                return

            start_reference = recording_start or time.perf_counter()
            recording_end = time.perf_counter()
            recording_duration = recording_end - start_reference

            audio_start = time.perf_counter()
            audio = rec.stop()
            audio_end = time.perf_counter()
            audio_process_time = audio_end - audio_start

            if audio is not None and audio.size > 0:
                try:
                    transcribe_start = time.perf_counter()
                    with transcribe_lock:
                        text = tx.transcribe(audio, output=args.output)
                    transcribe_end = time.perf_counter()
                except Exception as exc:
                    print(f"Error: {exc}", file=sys.stderr)
                    text = ""
                else:
                    transcribe_time = transcribe_end - transcribe_start

                    if args.benchmark and text:
                        processing_time = audio_process_time + transcribe_time
                        total_time = recording_duration + processing_time
                        timings.append({
                            'recording': recording_duration,
                            'audio_processing': audio_process_time,
                            'transcription': transcribe_time,
                            'processing_total': processing_time,
                            'total': total_time
                        })

                        print(f"\n⏱️  Timing breakdown:", file=sys.stderr)
                        print(f"  Recording duration: {recording_duration*1000:.0f}ms", file=sys.stderr)
                        print(f"  Audio processing: {audio_process_time*1000:.0f}ms", file=sys.stderr)
                        print(f"  Transcription: {transcribe_time*1000:.0f}ms", file=sys.stderr)
                        print(f"  → Processing time: {processing_time*1000:.0f}ms (what matters!)", file=sys.stderr)

                        if len(timings) >= 3:
                            avg_processing = sum(t['processing_total'] for t in timings[-5:]) / min(5, len(timings))
                            print(f"📊 Avg processing (last 5): {avg_processing*1000:.0f}ms", file=sys.stderr)

                if text:
                    if assistant and assistant.process_transcription(text):
                        if args.benchmark:
                            print(f"✅ Command processed", file=sys.stderr)
                    else:
                        if assistant and assistant.enabled:
                            text = assistant.format_for_app_context(text)
                        kbd.type(text)

            if wake_detector:
                wake_detector.resume()
            recording_start = None

    def on_chord(active: bool):
        try:
            if active:
                begin_recording()
            else:
                complete_recording()
        except Exception as e:
            print(f"Error: {e}", file=sys.stderr)

    if enable_vad_for_recording and endpoint_manager is not None:
        rec.on_auto_stop = lambda: threading.Thread(target=complete_recording, kwargs={"source": "vad"}, daemon=True).start()

    if wake_words_enabled:
        wake_config = build_wake_word_config(
            wake_words,
            window_seconds=args.wake_word_window_sec,
            min_gap_seconds=args.wake_word_gap_sec,
            vad_threshold=args.wake_word_vad_threshold,
            match_threshold=args.wake_word_match_threshold,
        )
        wake_detector = WakeWordDetector(
            tx,
            wake_config,
            device_name=args.device,
            on_detect=lambda: begin_recording("wake"),
            transcribe_lock=transcribe_lock,
        )
        wake_detector.start()

    listener = HotkeyListener(chord=chord, debounce_ms=args.debounce_ms, on_chord_active=on_chord)
    
    print("\n" + "="*60, file=sys.stderr)
    print("Ready! Hold chord to record, release to transcribe.", file=sys.stderr)
    if args.benchmark:
        print("Benchmark mode: Showing detailed timing breakdown", file=sys.stderr)
    print("="*60 + "\n", file=sys.stderr)
    
    try:
        listener.start()
        listener.join()
    except KeyboardInterrupt:
        listener.stop()
        print("\nStopped.", file=sys.stderr)

        if args.benchmark and timings:
            print("\n" + "="*60, file=sys.stderr)
            print("📈 SESSION SUMMARY", file=sys.stderr)
            print("="*60, file=sys.stderr)
            
            avg_processing = sum(t['processing_total'] for t in timings) / len(timings)
            min_processing = min(t['processing_total'] for t in timings)
            max_processing = max(t['processing_total'] for t in timings)
            
            avg_transcribe = sum(t['transcription'] for t in timings) / len(timings)
            avg_audio = sum(t['audio_processing'] for t in timings) / len(timings)
            
            print(f"Runs: {len(timings)}", file=sys.stderr)
            print(f"\nProcessing time (excluding recording):", file=sys.stderr)
            print(f"  Average: {avg_processing*1000:.0f}ms", file=sys.stderr)
            print(f"  Fastest: {min_processing*1000:.0f}ms", file=sys.stderr)
            print(f"  Slowest: {max_processing*1000:.0f}ms", file=sys.stderr)
            print(f"\nBreakdown averages:", file=sys.stderr)
            print(f"  Audio processing: {avg_audio*1000:.0f}ms", file=sys.stderr)
            print(f"  Transcription: {avg_transcribe*1000:.0f}ms", file=sys.stderr)
            
            print("\n💡 Performance tips:", file=sys.stderr)
            if args.model == "large-v3-turbo-q8_0":
                print("  • Try --model base.en for 3-5x faster transcription", file=sys.stderr)
            if rec.needs_resample:
                print("  • Audio resampling adds latency", file=sys.stderr)
            if args.debounce_ms > 30:
                print(f"  • Try --debounce-ms 30 to reduce delay by {args.debounce_ms-30}ms", file=sys.stderr)
    finally:
        if wake_detector:
            wake_detector.stop()
