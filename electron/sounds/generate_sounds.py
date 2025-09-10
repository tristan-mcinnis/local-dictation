#!/usr/bin/env python3
"""
Generate subtle, modern ping sounds for recording feedback
"""
import numpy as np
import soundfile as sf

def generate_ping(frequency=800, duration=0.15, sample_rate=44100):
    """Generate a subtle ping sound with envelope"""
    t = np.linspace(0, duration, int(sample_rate * duration))
    
    # Create a sine wave with harmonics for richness
    wave = (np.sin(2 * np.pi * frequency * t) * 0.6 +
            np.sin(2 * np.pi * frequency * 2 * t) * 0.2 +
            np.sin(2 * np.pi * frequency * 3 * t) * 0.1)
    
    # Apply envelope for smooth fade in/out
    envelope = np.exp(-t * 15) * np.sin(np.pi * t / duration) ** 0.5
    
    # Apply the envelope
    sound = wave * envelope * 0.3  # Keep volume low
    
    # Add slight reverb tail
    reverb_delay = int(0.02 * sample_rate)
    reverb = np.zeros(len(sound) + reverb_delay * 3)
    reverb[:len(sound)] = sound
    reverb[reverb_delay:reverb_delay+len(sound)] += sound * 0.3
    reverb[reverb_delay*2:reverb_delay*2+len(sound)] += sound * 0.15
    
    # Normalize
    reverb = reverb / np.max(np.abs(reverb)) * 0.3
    
    return reverb

def generate_stop_ping(frequency=600, duration=0.12, sample_rate=44100):
    """Generate a slightly lower, shorter ping for stop"""
    t = np.linspace(0, duration, int(sample_rate * duration))
    
    # Create a sine wave with slight pitch bend down
    pitch_bend = 1 - (t / duration) * 0.1
    wave = np.sin(2 * np.pi * frequency * pitch_bend * t)
    
    # Quicker envelope
    envelope = np.exp(-t * 20) * np.sin(np.pi * t / duration) ** 0.3
    
    sound = wave * envelope * 0.25
    
    return sound

# Generate the sounds
print("Generating recording start sound...")
start_sound = generate_ping(800, 0.15)
sf.write('record_start.wav', start_sound, 44100, subtype='PCM_16')

print("Generating recording stop sound...")
stop_sound = generate_stop_ping(600, 0.12)
sf.write('record_stop.wav', stop_sound, 44100, subtype='PCM_16')

print("Sounds generated successfully!")