# Performance Optimization Guide

## Current Performance Baseline (medium.en)
- Transcription: ~1200-1500ms
- Typing: ~60ms  
- Full cycle: 4-7 seconds

## Speed Optimization Options

### 1. Switch to Faster Model
```bash
# base.en: 3-4x faster (~300-400ms transcription)
uv run local-dictation --model base.en

# tiny.en: 5-10x faster (~100-200ms transcription)
uv run local-dictation --model tiny.en
```

**Trade-off**: Lower accuracy, especially for complex sentences

### 2. Force English Language (Skip Auto-Detection)
The logs show inconsistent language detection (da, fo, mi, zh), which adds overhead.

```python
# In src/local_dictation/transcribe.py, modify:
segments = self.model.transcribe(audio_array, language='en')
# Instead of letting it auto-detect
```

This saves ~50-100ms per transcription.

### 3. Use Large-v3-turbo Model
Counterintuitively, the turbo model can be faster with better accuracy:
```bash
uv run local-dictation --model large-v3-turbo
```

### 4. Punctuation Consistency
Whisper's punctuation is context-dependent. For consistent punctuation:
- Speak in complete sentences
- Add natural pauses where punctuation should go
- The model learns from speech patterns

## Recommended Settings

### For Maximum Speed (Sub-second transcription):
```bash
uv run local-dictation --model tiny.en --debounce-ms 30
```
- Total latency: ~1-2 seconds
- Good for quick notes

### For Best Balance (Current):
```bash
uv run local-dictation --model medium.en
```
- Total latency: ~4-5 seconds
- Excellent accuracy

### For Maximum Accuracy:
```bash
uv run local-dictation --model large-v3-turbo
```
- Total latency: ~5-6 seconds
- Best punctuation and accuracy

## Hardware Considerations

Your Apple Silicon is already using Metal acceleration. The ~1.3s transcription time for medium.en is near-optimal for this hardware.

## Is This Fast Enough?

**Yes, for most use cases:**
- 1.3 seconds transcription is competitive with cloud services
- No network latency
- Complete privacy
- Consistent performance

**For comparison:**
- Google Speech-to-Text: 500-1500ms (with network)
- Dragon NaturallySpeaking: 1000-2000ms
- macOS Dictation: 1000-3000ms (varies with network)

Your implementation is on par with or faster than most alternatives, especially considering it's 100% local.