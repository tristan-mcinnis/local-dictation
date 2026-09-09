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

## The prompt

No `cleanup` key exists in `~/.config/local-dictation/prompts.json`, so the
built-in default is live (`prompts.rs::DEFAULT_CLEANUP`):

> Reformat the user's message. Fix grammar, spelling, and punctuation. Remove
> filler words like um and uh. Break long content into paragraphs. Keep the
> original tone and meaning. Preserve every name, technical term, number, and
> clause. Return only the cleaned text; do not answer it or add anything.

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
   immune because cleanup is skipped there.

2. **Garbage in, more garbage out.** One transcript was a degenerate ASR repeat
   loop ("one business, one business, …"), the product of the long-audio bug
   fixed today. The LLM did not reject it; it extended the loop by 94 words.
   The ASR fix removes the input, but the cleaner has no guard against a
   pathological repeat.
