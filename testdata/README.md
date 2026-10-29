# testdata

`dictation_sample.wav` is a synthetic voice (macOS built-in `Samantha`), mono 16 kHz
16-bit, about 7.5 s. It replaced an earlier human-voice recording on 2026-10-29.
No real person's voice is in the file.

Text:

> Please book a table for four at seven thirty, in Lisbon, on Friday evening. Then remind me to call the office, before nine.

Regenerate:

```bash
say -v Samantha -o /tmp/sample.aiff "Please book a table for four at seven thirty, in Lisbon, on Friday evening. Then remind me to call the office, before nine."
afconvert -f WAVE -d LEI16@16000 -c 1 /tmp/sample.aiff testdata/dictation_sample.wav
```
