# Completion sounds

The selectable cue that plays after a dictation lands. Twenty audition
candidates, shipped in the binary via `include_bytes!` in `src/cues.rs` and
written to the per-user cue cache on first use. The stable id is the file
stem; the settings key is `completion_sound` and the human list is
`COMPLETION_SOUNDS` in `src/cues.rs`.

| Id | Label | Origin |
| --- | --- | --- |
| `tight-felt-thud` | 01 · Tight felt thud | Original synthesis |
| `compact-wood-thud` | 02 · Compact wood thud | Original synthesis |
| `soft-landing-thud` | 03 · Soft landing thud | Original synthesis |
| `deep-settle-thud` | 04 · Deep settle thud | Original synthesis |
| `muted-stamp` | 05 · Muted stamp | Original synthesis |
| `soft-ui-click` | 06 · Soft UI click | Original synthesis |
| `glass-tick` | 07 · Glass tick | Original synthesis |
| `warm-key-tap` | 08 · Warm key tap | Original synthesis |
| `bubble-pop` | 09 · Bubble pop | Original synthesis |
| `muted-pluck` | 10 · Muted pluck | Original synthesis |
| `double-settle` | 11 · Double settle | Original synthesis |
| `warm-done-tone` | 12 · Warm done tone | Original synthesis |
| `kenney-tight-click` | 13 · Tight click | Kenney Vleugels, CC0 |
| `kenney-bong-impact` | 14 · Bong impact | Kenney Vleugels, CC0 |
| `kenney-drop-impact` | 15 · Drop impact | Kenney Vleugels, CC0 |
| `kenney-soft-impact` | 16 · Soft impact | Kenney Vleugels, CC0 |
| `kenney-heavy-wood-impact` | 17 · Heavy wood impact | Kenney Vleugels, CC0 |
| `kenney-confirmation-1` | 18 · Confirmation 1 | Kenney Vleugels, CC0 |
| `kenney-confirmation-2` | 19 · Confirmation 2 | Kenney Vleugels, CC0 |
| `kenney-confirmation-3` | 20 · Confirmation 3 | Kenney Vleugels, CC0 |

Ids 01–12 are original sounds, synthesized locally from deterministic
synthesis (not copied from Superwhisper or another dictation app). Ids 13–20
are CC0 sounds by Kenney Vleugels, decoded to 48 kHz WAV with no further lossy
step. The default when no selection is saved is the existing tight completion
thud (`resources/sounds/success.wav`), not one of the twenty.

Attribution for the CC0 files lives in `THIRD_PARTY_NOTICES.md` at the repo
root. All twenty were level-matched for a fair audition.
