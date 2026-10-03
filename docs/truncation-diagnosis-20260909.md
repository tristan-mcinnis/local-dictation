# Why long dictations lose text (2026-09-09)

The author reported dictation "cutting off a lot of text at the end". It is real,
it is reproducible, and it is not the hotkey, the tail capture or the injector.

## Root cause

The ASR (Cohere Transcribe Q4, `native/cohere-mlx`) silently drops speech when
one request carries more than about 40 seconds of audio. It does not error. It
returns a shorter, plausible transcript.

Measured with a 177 s `say`-generated script of 40 numbered markers, fed to the
shipped helper (`dist/Local Dictation.app/Contents/Resources/cohere-mlx-bridge`,
`~/Models/cohere-mlx-q4`):

| audio in one request | words out | markers found | markers missing |
|---|---|---|---|
| 15 s | 49 | 4 of 4 | none |
| 30 s | 88 | 7 of 7 | none |
| 40 s | 120 | 10 of 10 | none |
| 50 s | 126 | 10 of 12 | 8, 9 |
| 60 s | 124 | 10 of 14 | 8, 9, 11, 12 |
| 80 s | 107 | 9 of 19 | ten markers |
| 100 s | 111 | 9 of 20 | eleven markers |
| 177 s | 140 | 9 of 40 | most of the script |

Output saturates near 140 words no matter how long the audio is. Loss is not
only at the end: whole sentences vanish from the middle.

`max_tokens` is NOT the cause. 256, 1024 and 4096 give byte-identical output.
The limit is inside the model / the mlx-audio decode path.

## Why nothing protects against it

Two guards exist and both are off:

1. `native/cohere-mlx/Sources/CohereMLXBridge/main.swift:103` sets
   `chunkDuration: 1200` (seconds). mlx-audio's own long-form chunking is
   therefore disabled. Its README states the encoder holds about 6.7 minutes,
   which is true for position encoding but not for decode quality.
2. The streaming VAD is meant to split a hold into pause-bounded segments, one
   ASR request each. It almost never fires on long holds. From
   `/tmp/dictate-daemon.log` (501 utterances):

   | hold | n | mean segments | single segment |
   |---|---|---|---|
   | 60-74 s | 6 | 1.00 | 100% |
   | 75-89 s | 3 | 1.00 | 100% |
   | 105-134 s | 4 | 1.00 | 100% |

   `vad::segment_speech` thresholds speech at `peak_frac * peak RMS` of the
   clip. `whisper_mode` (on in his `settings.json`) sets `peak_frac = 0.015`,
   about 36 dB below peak, so room noise never reads as silence and no 900 ms
   pause is ever detected. Long holds therefore go as one request, straight
   into the failure zone.

## Damage in the log

- 19 of 501 utterances carried more than 40 s of audio per ASR request.
- About 1440 words never arrived, against 14787 words injected: roughly 9%.
- Worst case: a 135 s hold returned 109 words, ending mid-thought, with a
  repeated clause ("I have my notes, which has all of my ... I have my notes").
  Repeated clauses and stalled endings are the signature of this failure.

## The fix

Cap the audio in any one ASR request. Chunking recovers the text and is also
faster:

| chunk size over the same 177 s | words | distinct markers |
|---|---|---|
| one request | 140 | 9 of 40 |
| 60 s | 397 | 25 of 40 |
| 30 s | 500 | 36 of 40 |

Recommended: split any buffer over about 30 s at its lowest-energy frame before
sending, in `transcriber` / `cohere_mlx`, independent of the VAD. That is a
floor that holds even when segmentation fails. Fixing `peak_frac` to an
absolute dBFS threshold is a good second step, but it must not be the only one.

## Second, smaller defect

The S1-mini cleanup deletes words. Log, 19:51:53, non-terminal target:

- raw: "the section for that given whatever it is. I mean, go back and look at"
- cleaned: "the section for that given whatever it go back and look at"

Cleanup is skipped in terminals, so this hits mail, Slack and browser fields
only. Separate issue, separate fix.

## How to reproduce

```bash
say -v Samantha -r 170 -o /tmp/speech.wav --data-format=LEI16@16000 \
    --file-format=WAVE -f /tmp/script.txt      # 40 numbered markers, 177 s
# feed prefixes of /tmp/speech.wav to the bridge over its JSON-line protocol
# and count how many "Marker N" survive
```
