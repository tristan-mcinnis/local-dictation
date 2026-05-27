//! Wake-word trigger corpus — shows the listen-mode trigger decision on
//! realistic raw-ASR transcripts: which segments fire (and what body they
//! yield) and which are correctly ignored as ambient speech.
//!
//! No models, no mic: the decision is the pure `wake_word::detect` primitive,
//! so this harness is the human-readable counterpart to its unit tests — a
//! quick way to eyeball trigger sensitivity and false-trigger resistance when
//! tuning the wake word.
//!
//! Run:  cargo run --release --example wake_word_lab
//!       cargo run --release --example wake_word_lab -- jarvis   # custom word

use fast_dictate_backend::wake_word::detect;

fn main() {
    let wake = std::env::args().nth(1).unwrap_or_else(|| "computer".to_string());

    // (transcript, should_fire) — a mix of real triggers and ambient speech
    // that must NOT fire (the segment a continuous listener would hear anyway).
    let corpus: &[(&str, bool)] = &[
        ("Computer, send the message", true),
        ("computer open my notes", true),
        ("hey computer what's the weather", true),
        ("Computers, delete that line", true), // ASR plural slip → still fires
        ("Computer.", true),                   // bare wake word → arms, empty body
        ("so then I told the computer to reboot", false), // mid-sentence → ignore
        ("let's grab lunch around noon", false),
        ("can you believe the game last night", false),
        ("um okay so the thing is", false),
    ];

    println!("wake word: \u{201C}{wake}\u{201D}\n");
    let mut wrong = 0;
    for &(text, should_fire) in corpus {
        let m = detect(text, &wake);
        let fired = m.is_some();
        let ok = fired == should_fire;
        if !ok {
            wrong += 1;
        }
        let mark = if ok { "ok " } else { "BAD" };
        match m {
            Some(w) if w.body.is_empty() => {
                println!("  [{mark}] FIRE (armed, no body)   ← {text:?}");
            }
            Some(w) => println!("  [{mark}] FIRE → {:?}   ← {text:?}", w.body),
            None => println!("  [{mark}] ignore                ← {text:?}"),
        }
    }
    println!("\n  {} / {} classified as expected", corpus.len() - wrong, corpus.len());

    println!(
        "\nNote: in listen mode every spoken segment above would first be \
         transcribed by Parakeet before this decision runs — that continuous \
         ASR is the real cost of always-listening. See docs/wake-word-and-preroll.md."
    );
    assert_eq!(wrong, 0, "wake-word corpus regressed");
}
