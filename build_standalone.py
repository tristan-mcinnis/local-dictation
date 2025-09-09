#!/usr/bin/env python3
"""
Build standalone executable for the Electron app
"""
import os
import shutil
import subprocess
import sys

def build_standalone():
    """Build standalone Python executable using PyInstaller"""
    
    # Install PyInstaller if not already installed
    subprocess.run(["uv", "add", "--dev", "pyinstaller"], check=True)
    
    # Create spec file for PyInstaller
    spec_content = '''
# -*- mode: python ; coding: utf-8 -*-

a = Analysis(
    ['src/local_dictation/cli_electron.py'],
    pathex=[],
    binaries=[],
    datas=[
        ('src/local_dictation/*.py', 'local_dictation'),
    ],
    hiddenimports=[
        'pynput',
        'pynput.keyboard',
        'pynput.keyboard._darwin',
        'sounddevice',
        'numpy',
        'scipy',
        'scipy.signal',
        'pywhispercpp',
        '_sounddevice_data',
    ],
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[],
    noarchive=False,
)

pyz = PYZ(a.pure)

exe = EXE(
    pyz,
    a.scripts,
    a.binaries,
    a.datas,
    [],
    name='local-dictation',
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=True,
    upx_exclude=[],
    runtime_tmpdir=None,
    console=True,
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch='arm64',
    codesign_identity=None,
    entitlements_file=None,
    icon=None,
)
'''
    
    with open('local_dictation.spec', 'w') as f:
        f.write(spec_content)
    
    # Build the executable
    print("Building standalone executable...")
    subprocess.run(["uv", "run", "pyinstaller", "local_dictation.spec", "--clean"], check=True)
    
    # Copy to Electron resources
    dist_dir = "dist"
    electron_dist = "electron/dist_python"
    
    if os.path.exists(electron_dist):
        shutil.rmtree(electron_dist)
    
    shutil.copytree(dist_dir, electron_dist)
    
    print(f"✅ Standalone executable built and copied to {electron_dist}")
    
    # Clean up
    if os.path.exists("build"):
        shutil.rmtree("build")
    if os.path.exists("local_dictation.spec"):
        os.remove("local_dictation.spec")

if __name__ == "__main__":
    build_standalone()