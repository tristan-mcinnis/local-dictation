use super::*;

#[test]
fn join_stream_segments_repairs_mid_sentence_boundaries() {
    let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    // The reported bug: a pause before the last word → spurious period.
    assert_eq!(
        join_stream_segments(&s(&["it's like instant.", "connection."])),
        "it's like instant connection."
    );
    // Capitalized start = a genuine new sentence — period stays.
    assert_eq!(
        join_stream_segments(&s(&["The...", "They can be paired."])),
        "The... They can be paired."
    );
    // Trail-off ellipsis is deliberate — never popped.
    assert_eq!(
        join_stream_segments(&s(&["I was thinking...", "maybe later."])),
        "I was thinking... maybe later."
    );
    // '!'/'?' are real terminals — untouched even before a lowercase start.
    assert_eq!(
        join_stream_segments(&s(&["Ready?", "yes."])),
        "Ready? yes."
    );
    // Empty / whitespace segments are dropped; single segment passes through.
    assert_eq!(join_stream_segments(&s(&["", "  ", "Hello."])), "Hello.");
    assert_eq!(join_stream_segments(&s(&["Hello."])), "Hello.");
}

#[test]
fn join_stream_segments_repairs_cuts_after_non_terminal_words() {
    let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    // 2026-09-03 live dictation: a breath after "of" before an acronym.
    // The period goes; the acronym keeps its capitals.
    assert_eq!(
        join_stream_segments(&s(&["The chat window at the bottom of.", "Of RTI."])),
        "The chat window at the bottom of of RTI."
    );
    // Everyday next word is lowercased.
    assert_eq!(
        join_stream_segments(&s(&["The light mode, which.", "I know right now."])),
        "The light mode, which I know right now."
    );
    assert_eq!(
        join_stream_segments(&s(&["It shifts up a little bit, which.", "Is weird."])),
        "It shifts up a little bit, which is weird."
    );
    // A name after a preposition keeps its capital.
    assert_eq!(
        join_stream_segments(&s(&["Send it to.", "Claude tomorrow."])),
        "Send it to Claude tomorrow."
    );
    // A real sentence end on a content word is untouched.
    assert_eq!(
        join_stream_segments(&s(&["Also look at the dictation.", "It keeps breaking."])),
        "Also look at the dictation. It keeps breaking."
    );
    // Trail-off after a function word is deliberate.
    assert_eq!(
        join_stream_segments(&s(&["I just want...", "Kath, I just want to know."])),
        "I just want... Kath, I just want to know."
    );
}

#[test]
fn strips_chat_template_artefacts() {
    assert_eq!(polish("Hello world.<end_of_turn>"), "Hello world.");
    assert_eq!(polish("Hello world.</s>"), "Hello world.");
}

#[test]
fn strips_reasoning_blocks_closed_and_unclosed() {
    // Closed block: keep what follows the closing tag.
    assert_eq!(
        polish("<think>the user wants a greeting</think>\nHello"),
        "Hello"
    );
    // Unclosed block (model cut off mid-reason): drop everything.
    assert_eq!(polish("<think>"), "");
    assert_eq!(polish("<think>let me consider"), "");
    // DeepSeek-style tags, case-insensitive.
    assert_eq!(
        polish("<Think>reasoning</Think>Answer"),
        "Answer"
    );
    // No tag → untouched.
    assert_eq!(polish("plain answer"), "plain answer");
    // A mid-sentence literal is never a reasoning block.
    assert_eq!(polish("I think this is fine"), "I think this is fine");
}

#[test]
fn strips_known_preambles() {
    assert_eq!(
        polish("Here's the cleaned text: We should ship."),
        "We should ship."
    );
    assert_eq!(polish("Sure! We should ship."), "We should ship.");
}

#[test]
fn strips_repeated_s1_role_labels() {
    assert_eq!(polish("Assistant Assistant: I sent it."), "I sent it.");
    assert_eq!(polish("Utterance: test this."), "test this.");
}

#[test]
fn peels_wrapping_quotes() {
    assert_eq!(polish("\"We should ship.\""), "We should ship.");
    assert_eq!(polish("'Hello world'"), "Hello world");
}

#[test]
fn peels_markdown_emphasis() {
    assert_eq!(polish("**We should ship.**"), "We should ship.");
    assert_eq!(polish("*hello*"), "hello");
}

#[test]
fn collapses_internal_whitespace() {
    assert_eq!(polish("  We  should   ship.  "), "We should ship.");
    assert_eq!(polish("line one\n\nline two"), "line one line two");
}

#[test]
fn preserves_intentional_content() {
    assert_eq!(
        polish("We should ship Rust + macOS code."),
        "We should ship Rust + macOS code."
    );
}

#[test]
fn empty_in_empty_out() {
    assert_eq!(polish(""), "");
    assert_eq!(polish("   \n  "), "");
}

#[test]
fn mechanics_drops_leading_and_inline_interjections() {
    assert_eq!(
        fix_speech_mechanics("so um I think we should ship it you know"),
        "So I think we should ship it you know."
    );
    // "Um" dropped as an interjection; leading "yeah," stripped as
    // throat-clearing; "so" (no comma) is a real connective, kept.
    assert_eq!(
        fix_speech_mechanics("Um, yeah, so the cache keeps growing."),
        "So the cache keeps growing."
    );
    assert_eq!(
        fix_speech_mechanics("uh we never evict anything"),
        "We never evict anything."
    );
}

#[test]
fn mechanics_capitalizes_standalone_i() {
    assert_eq!(
        fix_speech_mechanics("i don't know if i should"),
        "I don't know if I should."
    );
    assert_eq!(
        fix_speech_mechanics("i'm not sure i'll make it"),
        "I'm not sure I'll make it."
    );
}

#[test]
fn mechanics_leaves_words_containing_filler_or_i_alone() {
    // "i" inside a word and filler substrings inside real words survive
    // (only the sentence-initial letter is capitalized; "api"/"main" stay).
    assert_eq!(
        fix_speech_mechanics("the api summary is in the main file"),
        "The api summary is in the main file."
    );
    // "summary" contains "um"; "main" contains "i"; none are standalone.
    // (Sentence-initial capitalization makes it "Summary".)
    assert!(fix_speech_mechanics("summary").contains("Summary"));
}

#[test]
fn strips_leaked_list_intro() {
    assert_eq!(
        polish_multiline("Here's a bulleted list of the points:\n\n* The timeline is tight.\n* The budget is unclear."),
        "* The timeline is tight.\n* The budget is unclear."
    );
    assert_eq!(
        polish_multiline("Sure! Steps:\n1. Build it.\n2. Ship it."),
        "1. Build it.\n2. Ship it."
    );
}

#[test]
fn keeps_real_leading_line_before_list() {
    // A genuine sentence that isn't a chatty intro stays put.
    let input = "We have three options to weigh carefully.\n- Option A\n- Option B";
    assert_eq!(polish_multiline(input), input);
}

#[test]
fn peels_triple_backtick_fence() {
    assert_eq!(
        polish_multiline("```\n1. First\n2. Second\n```"),
        "1. First\n2. Second"
    );
    // With a language tag on the opening fence.
    assert_eq!(polish_multiline("```rust\nlet x = 1;\n```"), "let x = 1;");
}

#[test]
fn peels_double_backtick_fence() {
    assert_eq!(
        polish("``\nfix: cache growth on macOS.\n``"),
        "fix: cache growth on macOS."
    );
}

#[test]
fn leaves_inline_backticks_untouched() {
    // Not a whole-output fence — must survive.
    assert_eq!(polish("run `git status` first"), "run `git status` first");
}

#[test]
fn multiline_preserves_list_newlines() {
    // The transform path must keep line structure that `polish` flattens.
    assert_eq!(
        polish_multiline("* Buy milk\n* Buy eggs\n* Buy bread"),
        "* Buy milk\n* Buy eggs\n* Buy bread"
    );
}

#[test]
fn multiline_collapses_intra_line_spaces_and_extra_blanks() {
    assert_eq!(
        polish_multiline("para one\n\n\n\npara  two"),
        "para one\n\npara two"
    );
}

#[test]
fn multiline_still_strips_preamble_and_artefacts() {
    assert_eq!(
        polish_multiline("Sure! line one\nline two<end_of_turn>"),
        "line one\nline two"
    );
}

#[test]
fn strips_standalone_trailing_filler() {
    assert_eq!(
        strip_trailing_filler("the models we can use. Yeah."),
        "the models we can use."
    );
    assert_eq!(strip_trailing_filler("do it. yeah"), "do it.");
    assert_eq!(strip_trailing_filler("that's cool, yeah"), "that's cool.");
    assert_eq!(
        strip_trailing_filler("ship it tomorrow. You know."),
        "ship it tomorrow."
    );
}

#[test]
fn keeps_meaningful_and_whole_utterance_yeah() {
    // no clause boundary before "yeah" → it's part of the sentence
    assert_eq!(strip_trailing_filler("I told him yeah"), "I told him yeah");
    // the entire utterance is the acknowledgement → the speaker meant it
    assert_eq!(strip_trailing_filler("Yeah."), "Yeah.");
    assert_eq!(strip_trailing_filler("yeah"), "yeah");
    // whole-word only: must not chop the end of a real word
    assert_eq!(
        strip_trailing_filler("the deal is conveyed"),
        "the deal is conveyed"
    );
}

#[test]
fn detects_lists_vs_prose() {
    assert!(looks_like_list("1. Buy milk.\n2. Buy eggs.\n3. Buy flour."));
    assert!(looks_like_list("- one\n- two\n- three"));
    assert!(!looks_like_list(
        "I went to the store and bought milk, eggs, and flour."
    ));
    assert!(!looks_like_list(
        "First we set up the repo. Second we ship it."
    )); // one line
    assert!(!looks_like_list("3.11 is the version we shipped today.")); // single line, not a list
}

#[test]
fn flatten_collapses_newlines() {
    assert_eq!(flatten_to_line("a\nb   c\n\n d"), "a b c d");
}

#[test]
fn fix_speech_mechanics_drops_trailing_yeah_end_to_end() {
    // (leading "so" removal is the cleanup model's job, not this pass)
    assert_eq!(
        fix_speech_mechanics("i think we should ship it. yeah"),
        "I think we should ship it."
    );
}

#[test]
fn dropped_content_catches_a_deleted_clause() {
    // The real failure: a whole clause silently deleted (rewrite, not edit).
    let raw = "Hey Chris, just blocking your time to talk about basketball. Let me know if this works.";
    let gutted = "Hey Chris, let me know if this works.";
    assert!(cleanup_dropped_content(raw, gutted));
}

#[test]
fn dropped_content_ignores_legit_cleanup() {
    // Filler removal + light edits must NOT trip the guard.
    let raw = "um so like I think we should uh ship the the build tomorrow you know";
    let clean = "I think we should ship the build tomorrow.";
    assert!(!cleanup_dropped_content(raw, clean));

    // Contraction expansion / number formatting shouldn't fire either.
    let raw2 = "we're gonna wanna refactor this in version two point one soon";
    let clean2 = "We are going to want to refactor this in version 2.1 soon.";
    assert!(!cleanup_dropped_content(raw2, clean2));

    // A faithful, same-length clean is obviously fine.
    let raw3 = "the latency lab shows the model warms up once at boot";
    let clean3 = "The latency lab shows the model warms up once at boot.";
    assert!(!cleanup_dropped_content(raw3, clean3));
}

#[test]
fn dropped_content_catches_a_clause_dropped_from_a_longer_utterance() {
    // The log smoking gun: a content sentence deleted mid-utterance. Overall
    // length barely moves (64% retained), so the old char-ratio gate missed
    // it, but a run of content words ("amazing nice looking designs") is gone.
    let raw = "So I forked this repo and now this is me I'm working on it. So actually \
            what I would like is there are a lot of amazing nice looking designs in here. \
            I would like to make an HTML template design for the acme labs slide master that we have";
    let gutted = "So I forked this repo and now I'm working on it. Actually, I would like \
            to create an HTML template design for the Acme Labs slide master that we have.";
    assert!(cleanup_dropped_content(raw, gutted));
}

#[test]
fn dropped_content_skips_short_utterances() {
    // Too short to judge — never fire (avoids nuking genuinely terse edits).
    assert!(!cleanup_dropped_content(
        "yeah sounds good to me",
        "Sounds good."
    ));
}

#[test]
fn messiness_gate_skips_clean_speech() {
    // The exact failure case: clean, no mess → skip the LLM, so it can't
    // turn "open pencil" into "open source".
    assert!(!needs_llm_cleanup(
        "I don't use open pencil, you can remove that."
    ));
    assert!(!needs_llm_cleanup(
        "So that we could then update the basketball room slide sketch."
    ));
    assert!(!needs_llm_cleanup(
        "Give me the key structure that would appear inside a brief."
    ));
}

#[test]
fn strips_leading_throat_clearing_yeah() {
    assert_eq!(
        fix_speech_mechanics("Yeah, let's get the redesign live right now."),
        "Let's get the redesign live right now."
    );
    assert_eq!(
        fix_speech_mechanics("So, actually that might add latency."),
        "Actually that might add latency."
    );
    // Strips at most one leading filler.
    assert_eq!(
        fix_speech_mechanics("Yeah, okay, makes sense."),
        "Okay, makes sense."
    );
}

#[test]
fn capitalizes_sentence_start_after_model_strips_filler() {
    // The LLM stripped "Okay," itself and left "the" lowercase — repair it.
    assert_eq!(
        fix_speech_mechanics("the issue is when you look at some of the logs."),
        "The issue is when you look at some of the logs."
    );
    // Mid-utterance sentence start after a period.
    assert_eq!(
        fix_speech_mechanics("Cool, that makes sense. the other thing is the gutters."),
        "Cool, that makes sense. The other thing is the gutters."
    );
}

#[test]
fn capitalization_preserves_intercaps_and_decimals() {
    // Intercaps brand/identifier tokens at a sentence start stay verbatim.
    assert_eq!(
        capitalize_sentence_starts("iOS is fine. macOS too."),
        "iOS is fine. macOS too."
    );
    // A decimal after a period must not trigger a spurious capitalization.
    assert_eq!(
        capitalize_sentence_starts("Ship version 2.1 today."),
        "Ship version 2.1 today."
    );
    // Already-correct text is unchanged (idempotent).
    assert_eq!(
        capitalize_sentence_starts("Hello there. How are you?"),
        "Hello there. How are you?"
    );
}

#[test]
fn keeps_meaningful_leading_words() {
    // No comma ⇒ "so" is a real connective, leave it.
    assert_eq!(
        fix_speech_mechanics("So we need to ship the build."),
        "So we need to ship the build."
    );
    // A whole-utterance "Yeah." (no comma) is the speaker's actual answer.
    assert_eq!(fix_speech_mechanics("Yeah."), "Yeah.");
    // "right" mid-word / as content must not be touched.
    assert_eq!(
        fix_speech_mechanics("Right now is a good time."),
        "Right now is a good time."
    );
}

#[test]
fn messiness_gate_routes_messy_speech_to_llm() {
    assert!(needs_llm_cleanup(
        "we're gonna wanna refactor this kinda soon"
    )); // contractions
    assert!(needs_llm_cleanup("I think the the build is slow")); // stutter / false start
    assert!(needs_llm_cleanup(
        "it was like really slow and like you know kind of broken"
    )); // dense soft filler
        // A 40+ word run-on with no sentence breaks needs re-segmenting.
    let runon = "so then we did the thing and then the other thing and then \
            after that we went over to the place and grabbed the stuff and then \
            came back and started again with the first part of the whole process once more";
    assert!(needs_llm_cleanup(runon));
}

#[test]
fn self_correction_drops_abandoned_word_and_marker() {
    // The user's exact reported cases.
    assert_eq!(
        apply_self_correction("Let's meet Monday. No wait, I mean Tuesday."),
        "Let's meet Tuesday."
    );
    assert_eq!(
        apply_self_correction("Let's meet Monday, no wait, I mean Tuesday."),
        "Let's meet Tuesday."
    );
    assert_eq!(
        apply_self_correction("The report is due Friday, actually I mean Monday."),
        "The report is due Monday."
    );
    assert_eq!(
        apply_self_correction("ship it, I mean the release"),
        "ship the release"
    );
    // No marker, or marker without content before it → unchanged.
    assert_eq!(apply_self_correction("I mean well"), "I mean well");
    assert_eq!(
        apply_self_correction("I mean, the release is ready"),
        "I mean, the release is ready"
    );
    assert_eq!(
        apply_self_correction("the report is due Monday"),
        "the report is due Monday"
    );
}

#[test]
fn clean_email_with_enumerated_priorities_stays_faithful_prose() {
    let raw = "Okay, this is me dictating an email to Alex, hoping that they have a good day at work tomorrow. There are three things I want to focus on. The first one is signing a new deal, the second one is improving relations, and the third is planning a trip again in the fall. And those are the key priorities. So, thanks so much. Best, Sam.";

    assert_eq!(
        fix_speech_mechanics(raw),
        "This is me dictating an email to Alex, hoping that they have a good day at work tomorrow. There are 3 things I want to focus on. The first one is signing a new deal, the second one is improving relations, and the third is planning a trip again in the fall. And those are the key priorities. So, thanks so much. Best, Sam."
    );
}

#[test]
fn messiness_gate_ignores_ordinary_non_repair_uses() {
    // "actually"/"sorry"/"no" in ordinary, non-repair senses must NOT route
    // to the LLM — the deterministic pass handles them.
    assert!(!needs_llm_cleanup("actually I like that"));
    assert!(!needs_llm_cleanup("I'm sorry to hear that"));
    assert!(!needs_llm_cleanup("no problem at all"));
    // Emphasis use of "actually" (pronoun before it) isn't a repair.
    assert!(!needs_llm_cleanup("I actually like that idea"));
    assert!(!needs_llm_cleanup("we actually shipped it yesterday"));
}

// -------------------------------------------------------------------
// New deterministic rules (2–4): filler quote-safety, repeated-fragment
// collapse, spoken technical normalization, and the trailing stop.
// -------------------------------------------------------------------

#[test]
fn filler_removal_is_quote_aware() {
    // Standalone filler, mid-clause, is still dropped (with seam repair).
    assert_eq!(
        fix_speech_mechanics("that, um, we should ship it"),
        "That, we should ship it."
    );
    // Filler inside a quoted span is verbatim speech → kept, quotes balanced.
    assert_eq!(
        fix_speech_mechanics("He said \"um, yeah\" and left"),
        "He said \"um, yeah\" and left."
    );
    // A filler substring inside a real word is never touched.
    assert_eq!(
        fix_speech_mechanics("the summary and the armour are here"),
        "The summary and the armour are here."
    );
}

#[test]
fn collapse_repeated_fragments_collapses_stutter() {
    assert_eq!(collapse_repeated_fragments("I I think"), "I think");
    assert_eq!(
        collapse_repeated_fragments("we should we should go"),
        "we should go"
    );
    assert_eq!(
        collapse_repeated_fragments("the the build is slow"),
        "the build is slow"
    );
    // Never crosses a sentence boundary or a non-adjacent repeat.
    assert_eq!(
        collapse_repeated_fragments("we should. we should go"),
        "we should. we should go"
    );
}

#[test]
fn collapse_repeated_fragments_keeps_intentional_repeats() {
    assert_eq!(collapse_repeated_fragments("very very good"), "very very good");
    assert_eq!(collapse_repeated_fragments("no no"), "no no");
    assert_eq!(collapse_repeated_fragments("that that is true"), "that that is true");
    assert_eq!(collapse_repeated_fragments("had had enough"), "had had enough");
    // A figure is kept, not collapsed — be conservative, prefer allowing.
    assert_eq!(collapse_repeated_fragments("fifty fifty"), "fifty fifty");
}

#[test]
fn spoken_technical_tilde_and_dots() {
    assert_eq!(normalize_spoken_dots("redirect to tilde slash"), "redirect to ~/");
    assert_eq!(normalize_spoken_dots("open index dot js"), "open index.js");
    assert_eq!(normalize_spoken_dots("the file dot js"), "the file.js");
    assert_eq!(normalize_spoken_dots("visit example dot com"), "visit example.com");
    assert_eq!(normalize_spoken_dots("dot js"), ".js");
    assert_eq!(normalize_spoken_dots("dot dot dot"), "…");
}

#[test]
fn spoken_identifier_joins() {
    assert_eq!(normalize_spoken_joins("foo underscore bar"), "foo_bar");
    assert_eq!(normalize_spoken_joins("pull dash request"), "pull-request");
    assert_eq!(normalize_spoken_joins("type hyphen hint"), "type-hint");
    // Prose / ordinary English must NOT fire (function-word flanking words).
    assert_eq!(normalize_spoken_joins("the dash is missing"), "the dash is missing");
    assert_eq!(normalize_spoken_joins("a underscore b"), "a underscore b");
    assert_eq!(normalize_spoken_joins("Foo underscore bar"), "Foo_bar");
}

#[test]
fn spoken_at_sign_and_email() {
    assert_eq!(normalize_at("at sign is used"), "@ is used");
    assert_eq!(normalize_at("john at gmail dot com"), "john@gmail.com");
    // Prose "at sign" is left alone (a content word precedes "at").
    assert_eq!(normalize_at("look at sign"), "look at sign");
    // Ordinary prose describing a domain must not become an email.
    assert_eq!(normalize_at("I looked at domain dot com"), "I looked at domain dot com");
}

#[test]
fn spoken_versions_are_numbers_concern() {
    // "two point one" → "2.1" is numbers.rs' job; we deliberately add no
    // version logic here. Confirm the deterministic path still renders it.
    assert_eq!(fix_speech_mechanics("version two point one"), "Version 2.1");
}

#[test]
fn trailing_stop_turns_prose_into_sentences() {
    assert_eq!(add_terminal_period("The cat sat down"), "The cat sat down.");
    assert_eq!(fix_speech_mechanics("open the settings file"), "Open the settings file.");
    // Too short for a sentence → no period.
    assert_eq!(add_terminal_period("Hello world"), "Hello world");
    // Already punctuated → unchanged.
    assert_eq!(add_terminal_period("The cat sat."), "The cat sat.");
    assert_eq!(add_terminal_period("The cat sat!"), "The cat sat!");
    // A list is not prose.
    assert_eq!(
        add_terminal_period("1. Buy milk\n2. Buy eggs"),
        "1. Buy milk\n2. Buy eggs"
    );
    // A trailing voice command is left for the refiner.
    assert_eq!(
        fix_speech_mechanics("run the build press enter"),
        "Run the build press enter"
    );
}

#[test]
fn new_deterministic_rules_are_idempotent() {
    for input in [
        "I I think we should ship it",
        "we should we should go",
        "very very good",
        "He said \"um, yeah\" and left",
        "foo underscore bar",
        "visit example dot com",
        "dot dot dot",
        "open the settings file",
    ] {
        let once = fix_speech_mechanics(input);
        let twice = fix_speech_mechanics(&once);
        assert_eq!(once, twice, "idempotence failed for {input:?}");
    }
}
