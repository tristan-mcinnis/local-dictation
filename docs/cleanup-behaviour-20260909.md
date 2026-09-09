# Is the S1-mini cleanup actually doing anything? (2026-09-09)

Short answer: yes, it works, and it almost never runs for the user.

## When cleanup is activated

Four gates, in order (`daemon/worker.rs`, the `final_text` block):

1. **Target app is a terminal → skipped entirely.** Raw ASR text is injected.
2. **Under 3 words → deterministic only.** Nothing for a model to fix in "Yes."
3. **`text_polish::needs_llm_cleanup` says the text is already clean →
   deterministic only.** It returns true only on: a colloquial contraction
   (gonna, wanna, kinda…), an immediate repeated word, two or more soft fillers
   ("like", "you know", "I mean", "basically"…), or a 40-word-plus run-on with
   at most one full stop.
4. Otherwise the S1-mini LLM runs, and a **diff-guard**
   (`text_polish::cleanup_dropped_content`) throws its output away and keeps the
   deterministic text if a whole clause went missing.

## What actually happened, in the log

540 utterances in `/tmp/dictate-daemon.log`:

| path | count | share |
|---|---|---|
| terminal → skipped | 443 | 82% |
| deterministic only | 72 | 13% |
| reached the LLM | 21 | 4% |
| diff-guard fired | 0 | 0% |

The user works in Ghostty, so four dictations in five never touch cleanup at all.
Every "cleanup did nothing" impression is that rule, not a broken model.

## Before and after, on 429 of his own raw transcripts

`examples/cleanup_lab.rs` replays the production gating over the real raw
transcripts pulled from the log:

```bash
cargo run --release --features full --example cleanup_lab -- /tmp/raw_corpus.json
```

| result | count |
|---|---|
| looked clean → deterministic only | 309 |
| sent to the LLM | 120 |
| LLM changed the text | 66 |
| LLM returned the text unchanged | 54 |
| diff-guard rejected the output | 0 |
| mean LLM latency | 421 ms |

So of 429 real dictations the LLM would have altered 66, about 15%. Mean word
delta on those: −2.1. It is doing exactly the small job it is prompted for.

Typical edits (real, from the run):

- "I'm gonna go to sleep" → "I'm going to go to sleep"
- "which like kind of fucks up the overall system that I'm like trying to
  develop" → "which kind of fucks up the overall system that I'm trying to
  develop"
- "in slash document slash code" → "in /document/code"
- "quote unquote power user" → "quote-unquote power user"

## The prompt, and why it changed today

No `cleanup` key exists in `~/.config/local-dictation/prompts.json`, so the
built-in default is what runs. The old one opened "Reformat the user's
message", which a 1B model reads as permission to rewrite, and it took it.

Superwhisper ships the same S1-mini and solves this in the prompt. Its live
cleanup prompt, read out of `/Applications/superwhisper.app` (it is on this
Mac), frames the job as the *smallest possible change*, gives an explicit
allow-list of what may be touched, and then forbids the failure directly:

> Do not rewrite or improve the user's phrasing. Keep every other word and
> preserve the user's tone, uncertainty, slang, and emotional intensity. Never
> soften or remove profanity. Keep every clause the speaker said […] Never
> delete part of the message to make the rest read as a tidy sentence.

It also applies a separate Tone instruction (formal / semi-formal / semi-casual
/ casual) that is explicitly allowed to change casing, punctuation and word
forms only.

That shape was measured, not assumed. Three prompts over the same 429
transcripts, counting non-filler words the LLM deleted (spelled numbers
excluded, since digits are the intended output):

| prompt | utterances that lost a content word | content words lost | latency |
|---|---|---|---|
| old default ("Reformat the user's message…") | 25, 25, 27 of 120 | 52, 52, 57 | 453 ms |
| Superwhisper's, verbatim | 19 of 120 | 34 | 423 ms |
| **our rewrite of the same principles (now shipped)** | **18, 18, 21, 22 of 120** | **29, 42, 50, 52** | **421 ms** |

Repeated runs, because S1 is not bit-identical between runs even at
temperature 0. Read them honestly:

- **The utterance count separates cleanly.** Old 25–27, new 18–22. Fewer
  dictations come back damaged, every run.
- **The word total overlaps at the top end.** Old 52–57, new 29–52. The new
  prompt is better on average, but a bad run of it costs about what a typical
  run of the old one did. This is an improvement, not a cure.

The new default is our own wording, not a copy of theirs. The worst old
failure, fourteen words deleted from a half-finished sentence to tidy the rest,
did not recur in any run of the new one.

The shipped default is now:

> Clean up the user's dictated message with the smallest change that works.
> You may only: fix punctuation, capitalization, obvious misspellings, and
> homophones; write clearly spoken numbers and dates as digits; delete "um",
> "uh", an accidentally repeated word, and an explicit self-correction; start a
> new paragraph where the message is long or turns to a new subject.
> Do not rephrase, tighten, summarize, or improve the wording. Keep every other
> word the speaker said, and keep their tone, hesitation, slang, and force.
> Never soften or cut profanity. Never drop a clause, an aside, or a trailing
> thought to make the rest read as a tidy sentence: an unfinished sentence
> stays unfinished.
> Return only the cleaned message. Do not answer it, and add nothing.

An output-format preset (numbered / bullets / email / code) replaces this
prompt entirely while it is active. Transform mode (Shift + push-to-talk) is a
separate, deliberately permissive prompt, and that one IS overridden in his
`prompts.json`.

## Two real defects found

1. **The LLM drops small connective words and the guard does not catch it.**
   Reproducible three times out of three:

   - in: "…the section for that given whatever **it is. I mean,** go back and
     look at what we did for Acme Amplify."
   - out: "…the section for that given whatever **it** go back and look at what
     we did for Acme Amplify."

   `cleanup_dropped_content` only fires on a whole missing clause, so a
   three-word drop passes. Hits mail, Slack and browser fields; terminals are
   immune because cleanup is skipped there. **The new prompt does not fix this
   one**: it cuts the aggregate loss roughly in half, but this sentence still
   comes out as "whatever it go back". The real fix is a stricter diff-guard,
   not more prompt text.

2. **Garbage in, more garbage out.** One transcript was a degenerate ASR repeat
   loop ("one business, one business, …"), the product of the long-audio bug
   fixed today. The LLM did not reject it; it extended the loop by 94 words.
   The ASR fix removes the input, but the cleaner has no guard against a
   pathological repeat.
