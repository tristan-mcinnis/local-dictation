# Always-on mic (pre-roll) & wake-word listen mode

Two opt-in features, both off by default. This doc records what they are, how to
turn them on, how they were validated, and — per the project's "measure, then
decide" rule — an honest verdict on whether each is worth running.

## TL;DR verdict

| Feature | Verdict | Why |
|---|---|---|
| **Always-on mic / pre-roll** | **Recommended — worth shipping on.** | Cheap primitive, directly fixes clipped first words *and* removes stream-open latency. Measured 300 ms of leading speech recovered in the synthetic harness. The only cost is a continuously-warm input stream (mic indicator stays on). |
| **Wake-word listen mode** | **Sensible as an opt-in experiment, not as an always-on default.** | The trigger logic is solid and false-trigger-resistant, but it requires *continuous Parakeet ASR* on every spoken segment all day. That's a real CPU/battery/thermal cost that fights the project's low-latency/low-power ethos. Great for a hands-free session you turn on deliberately; not something to leave running. |

Both are togglable, so you lose nothing by having them available.

---

## 1. Always-on mic with pre-roll

### What it does
Normally the daemon opens the microphone **on key-press**. That has two costs the
user hit directly:
1. **Stream-open latency** — CoreAudio takes ~100–300 ms to spin up a freshly
   opened input stream; audio doesn't flow until it's live.
2. **Clipped first words** — people start the first syllable *before* the
   modifier key fully registers.

Pre-roll keeps the input stream **warm for the daemon's whole life**, feeding a
small rolling ring (default 400 ms). On key-press the next audio callback flushes
that lookback into the transcript buffer *first*, then streams live audio. So the
transcript starts a few hundred ms before the press, and there's no open latency.

This is the cheapest primitive that solves it (a fixed-size `VecDeque`, pure DSP)
— exactly per `CLAUDE.md`'s design principle. It's `audio::AlwaysOnCapture` +
`audio::PrerollRing`.

### How to enable
- **Menu bar:** *Always-on mic (pre-roll)* toggle.
- **settings.json:** `"preroll_ms": 400` (any value > 0 enables; `0`/absent = current open-on-press).
- **Env:** `DICTATE_PREROLL_MS=400`.

Takes effect on daemon relaunch (`./scripts/reload-daemon.sh`).

### Cost / trade-offs
- The mic indicator (orange dot) stays on the whole time the daemon runs. That's
  inherent to keeping the stream warm — it's the price of zero-latency capture.
- One always-running CoreAudio callback resampling to 16 kHz. Negligible CPU
  (it's the same per-callback work the PTT path already does, just continuous),
  and **no ASR runs while idle** — idle callbacks only update the rolling ring,
  they don't push to the consumer, so Parakeet/Gemma stay asleep until a press.
- Mutually exclusive with the experimental `streaming_cleanup` (pre-roll wins).
- **Bluetooth headsets: not recommended.** Holding an input stream open on
  AirPods / a BT headset pins macOS into the low-quality HFP "call" codec for the
  stream's whole life, which also degrades that device's *output* (music/video
  sound bad) even when you're not dictating. `AlwaysOnCapture::start` logs a
  warning when the input device looks Bluetooth; prefer `preroll_ms = 0` there.
- **Default-input changes are handled.** The warm stream binds to whatever input
  was default at boot; if the user later switches inputs (AirPods connect, a call
  app grabs the mic), the daemon detects the change on the next key-press
  (`AlwaysOnCapture::device_changed`) and rebuilds the stream on the current
  device, so it never silently keeps recording the old one.

### Validation
- Unit tests: `PrerollRing` rolling/eviction semantics, `preroll_samples`,
  resample passthrough (`src/audio.rs`).
- Harness: `cargo run --release --example preroll_lab` — simulates both capture
  strategies on a synthetic utterance and prints the leading-speech each keeps.
  Result: **open-on-press loses 300 ms, pre-roll loses 0**.
- **Not yet validated on a real mic** (an agent can't grant Mic/Accessibility or
  hold the hotkey). Same status the `streaming_cleanup` feature shipped in. Try
  it with `DICTATE_PREROLL_MS=400` and confirm leading words survive.

---

## 2. Wake-word listen mode (the `listen` subcommand)

### What it does
A hands-free mode: the mic listens continuously, segments speech at VAD pauses,
transcribes each segment with the already-loaded Parakeet model, and watches for
a configured **wake word** (default `"computer"`). Hearing it "arms" dictation —
that segment's body and every following segment (until 8 s of silence) is
cleaned, refined, and injected into the focused field, exactly like push-to-talk.

It pairs with the existing trailing voice commands, which is the badass part the
user wanted: *"computer, reply on my way. Press enter."* → text injected, Enter
synthesized, message sent — no keyboard.

The trigger decision is the pure, unit-tested `wake_word::detect`: case- and
punctuation-insensitive, tolerant of a leading "hey/ok/okay", absorbs a
one-character ASR slip on the magic word, and requires the wake word to *lead*
the segment so mid-sentence mentions ("tell the computer to stop") don't fire.

### How to run
It's a **separate mode**, deliberately not bolted onto the PTT daemon as a
background watcher (see verdict below):

```bash
./target/release/fast-dictate-backend listen
# or with a custom word:
DICTATE_WAKE_WORD=jarvis ./target/release/fast-dictate-backend listen
```

Settings/env knobs (read at boot): `wake_word` / `DICTATE_WAKE_WORD`,
`listen_mode` / `DICTATE_LISTEN_MODE` (the menu-bar toggle sets `listen_mode`).

### Why it's an opt-in experiment, not an always-on default
This is where the "is it actually a good idea?" question lands honestly:

- **Continuous ASR is the real cost.** To detect a wake word you must transcribe
  *everything* you hear, all day. Parakeet TDT is fast per-segment, but running
  it on every VAD segment continuously keeps the ANE/GPU busy — measurable
  battery drain and heat on a laptop. That directly fights the project's
  low-latency/low-power, "don't add heavyweight machinery" ethos.
- **The cheaper-looking alternatives are worse fits.** A dedicated wake-word
  engine (Porcupine) needs a cloud access key (not local-first); openWakeWord is
  a Python/TFLite stack (a heavy new dependency + model). Reusing the Parakeet we
  already load is the most in-stack option — but it's the "always burning the
  model" option.
- **It's genuinely useful when deliberate.** For a hands-free stretch (cooking,
  notes while pacing) you turn it on, use it, turn it off. That's a great fit.
  Leaving it on as a daemon default is not.

So: shipped as a togglable, standalone mode. Mitigations already in place — VAD
gates ASR to actual speech (silence is free), and the arm window means you say
the wake word once and then dictate freely for a few seconds rather than
re-triggering each sentence.

### Validation
- Unit tests: `wake_word::detect` (11 cases — casing, punctuation, lead-ins,
  multi-word phrases, fuzzy slip, mid-sentence rejection, blank/empty), plus
  `vad::SegmentStream::compact` (bounded memory for a long listen session).
- Harness: `cargo run --release --example wake_word_lab` — runs a corpus of real
  triggers + ambient speech and shows what fires vs. is ignored (9/9 correct).
- **Not yet validated on a real mic.** Needs a live trial: false-trigger rate in
  real ambient conversation, and the VAD `peak_frac`/`min_pause_ms` may need
  re-tuning on noisy mic audio (the VAD was tuned on clean synthetic speech).

### Future work (if it proves its worth on real audio)
- Measure idle vs. listening power draw to put a real number on the cost.
- Consider a lightweight energy/keyword pre-gate so Parakeet only runs when a
  sound *might* be the wake word, cutting the continuous-ASR duty cycle.
- Only then consider integrating it into the main daemon as a concurrent
  background mode (mic/worker contention with PTT needs careful design).
