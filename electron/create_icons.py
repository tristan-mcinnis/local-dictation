#!/usr/bin/env python3
"""
Create icon assets for the Electron app
"""
from PIL import Image, ImageDraw
import os

def create_tray_icon():
    """Create a simple circle tray icon"""
    # Create a 22x22 image for macOS menu bar (Template image)
    size = 22
    img = Image.new('RGBA', (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    
    # Draw a simple circle
    # macOS template images use black pixels which get converted to white/gray
    circle_size = 10
    padding = (size - circle_size) // 2
    draw.ellipse([padding, padding, padding + circle_size, padding + circle_size], 
                fill=(0, 0, 0, 255))
    
    # Save as Template image (black pixels with alpha)
    os.makedirs('assets', exist_ok=True)
    img.save('assets/tray-icon.png')
    
    # Also create @2x version for Retina displays
    img2x = img.resize((44, 44), Image.Resampling.NEAREST)
    img2x.save('assets/tray-icon@2x.png')

def create_app_icon():
    """Create app icon in multiple sizes"""
    sizes = [16, 32, 64, 128, 256, 512, 1024]
    
    for size in sizes:
        img = Image.new('RGBA', (size, size), (0, 0, 0, 0))
        draw = ImageDraw.Draw(img)
        
        # Background circle
        padding = size // 8
        draw.ellipse([padding, padding, size - padding, size - padding], 
                    fill=(0, 122, 255, 255))
        
        # Microphone shape
        mic_width = size // 4
        mic_height = size // 2.5
        mic_x = (size - mic_width) // 2
        mic_y = size // 4
        
        # White microphone
        draw.ellipse([mic_x, mic_y, mic_x + mic_width, mic_y + mic_width // 2], 
                    fill=(255, 255, 255, 255))
        draw.rectangle([mic_x, mic_y + mic_width // 4, mic_x + mic_width, mic_y + mic_height], 
                       fill=(255, 255, 255, 255))
        
        # Stand
        stand_width = size // 16
        stand_x = (size - stand_width) // 2
        draw.rectangle([stand_x, mic_y + mic_height, stand_x + stand_width, size - size // 3], 
                       fill=(255, 255, 255, 255))
        
        # Base
        base_width = mic_width + size // 8
        base_x = (size - base_width) // 2
        draw.rectangle([base_x, size - size // 3, base_x + base_width, size - size // 4], 
                       fill=(255, 255, 255, 255))
        
        img.save(f'assets/icon_{size}.png')
    
    print("Icons created in assets/ directory")

if __name__ == "__main__":
    create_tray_icon()
    create_app_icon()
    print("✅ Icons created successfully")