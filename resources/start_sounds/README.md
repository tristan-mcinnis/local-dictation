# Start sounds

The selectable cue that plays when a recording starts. Seven audition
candidates, shipped in the binary via `include_bytes!` in `src/cues.rs` and
written to the per-user cue cache on first use. The stable id is the file stem;
the settings key is `start_sound` and the human list is `START_SOUNDS` in
`src/cues.rs`.

| Id | Label | Origin |
| --- | --- | --- |
| `soft-rise` | 01 · Soft rise | Original synthesis |
| `bright-rise` | 02 · Bright rise | Original synthesis |
| `low-rise` | 03 · Low rise | Original synthesis |
| `quick-blip` | 04 · Quick blip | Original synthesis |
| `double-tap` | 05 · Double tap | Original synthesis |
| `glass-rise` | 06 · Glass rise | Original synthesis |
| `warm-rise` | 07 · Warm rise | Original synthesis |

All seven are original, synthesized locally from deterministic synthesis by
`scripts/audio/generate_cues.py` (not copied from Superwhisper or another
dictation app). Every candidate rises in pitch, so a start cue and a completion
cue can never be confused by shape. The default when no selection is saved is
the existing rising pair (`resources/sounds/start.wav`), not one of the seven.
