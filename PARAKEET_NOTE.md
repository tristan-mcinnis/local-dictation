# Parakeet MLX Support Note

## Current Status
The MLX Parakeet transcription engine foundation has been added but is not yet fully functional due to Python version compatibility issues.

## Issue
- `mlx-audio` requires `llvmlite==0.36.0` which only supports Python 3.6-3.9
- Our project uses Python 3.12
- Direct MLX Parakeet inference requires additional work

## Future Implementation Options

1. **Use subprocess with Python 3.9 environment**
   - Create a separate Python 3.9 environment for Parakeet
   - Call it via subprocess from main app
   - Pros: Clean separation, full Parakeet features
   - Cons: Additional setup complexity

2. **Wait for mlx-audio updates**
   - Monitor for Python 3.12 compatibility updates
   - Cleanest solution once available

3. **Use alternative Parakeet implementations**
   - Explore other Parakeet ports that work with Python 3.12
   - Consider ONNX or CoreML versions

## Temporary Workaround
The `parakeet.py` module has been created with the basic structure. Currently, it falls back to Whisper when called. Once the compatibility issues are resolved, the module can be completed with proper MLX inference.

## Files Created
- `src/local_dictation/parakeet.py` - Basic Parakeet transcriber structure
- Model download logic implemented
- Integration points ready for when compatibility is resolved