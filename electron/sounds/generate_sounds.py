#!/usr/bin/env python3
"""
Generate subtle, modern ping sounds for recording feedback
"""
import numpy as np
import soundfile as sf

def generate_ping(frequency=700, duration=0.08, sample_rate=44100):
    """Generate a very subtle, soft ping sound with envelope"""
    t = np.linspace(0, duration, int(sample_rate * duration))

    # Create a softer sine wave with minimal harmonics for gentleness
    wave = (np.sin(2 * np.pi * frequency * t) * 0.7 +
            np.sin(2 * np.pi * frequency * 1.5 * t) * 0.2 +
            np.sin(2 * np.pi * frequency * 2.5 * t) * 0.1)

    # Apply gentler envelope for very smooth fade in/out
    envelope = np.exp(-t * 25) * np.sin(np.pi * t / duration) ** 0.8

    # Apply the envelope with lower volume
    sound = wave * envelope * 0.15  # Much quieter

    # Add very subtle reverb tail
    reverb_delay = int(0.015 * sample_rate)
    reverb = np.zeros(len(sound) + reverb_delay * 2)
    reverb[:len(sound)] = sound
    reverb[reverb_delay:reverb_delay+len(sound)] += sound * 0.2

    # Normalize to very low level
    reverb = reverb / np.max(np.abs(reverb)) * 0.2

    return reverb

def generate_stop_ping(frequency=550, duration=0.06, sample_rate=44100):
    """Generate a very subtle, lower ping for stop"""
    t = np.linspace(0, duration, int(sample_rate * duration))

    # Create a softer sine wave with gentle pitch bend down
    pitch_bend = 1 - (t / duration) * 0.05
    wave = np.sin(2 * np.pi * frequency * pitch_bend * t)

    # Very gentle envelope
    envelope = np.exp(-t * 30) * np.sin(np.pi * t / duration) ** 0.6

    sound = wave * envelope * 0.12  # Even quieter than start sound

    return sound

# Generate the sounds
print("Generating recording start sound...")
start_sound = generate_ping()  # Use default parameters now
sf.write('record_start.wav', start_sound, 44100, subtype='PCM_16')

print("Generating recording stop sound...")
stop_sound = generate_stop_ping()  # Use default parameters now
sf.write('record_stop.wav', stop_sound, 44100, subtype='PCM_16')

print("Sounds generated successfully!")