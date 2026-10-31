use super::*;

fn scratch_file(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fd-corr-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("corrections.json")
}

#[test]
fn a_corrupt_corrections_file_is_never_overwritten() {
    // Regression: a parse error opened the editor empty, and Save replaced
    // the user's whole dictionary with the one term they had just added.
    let path = scratch_file("corrupt");
    let broken = "{ \"to twist\": \"Todoist\",, \"aoife\": \"Aoife\" }";
    std::fs::write(&path, broken).unwrap();
    assert!(Corrections::load_from(&path).is_err());
    let one = Corrections::from_editor_text("Beatrix");
    let err = one.save_to(&path).unwrap_err().to_string();
    assert!(err.contains("could not be read"), "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn save_keeps_comment_keys_and_round_trips() {
    let path = scratch_file("comments");
    std::fs::write(
        &path,
        r#"{ "_comment": "names from the Q3 project", "to twist": "Todoist" }"#,
    )
    .unwrap();
    let mut c = Corrections::load_from(&path).unwrap();
    assert_eq!(c.len(), 1);
    c.import_csv("aoife,Aoife\n").unwrap();
    c.save_to(&path).unwrap();
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("names from the Q3 project"), "{body}");
    let back = Corrections::load_from(&path).unwrap();
    assert_eq!(back.len(), 2);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn a_blank_file_is_an_empty_dictionary_and_can_be_saved_over() {
    // Regression: a zero-byte corrections.json failed to load, so both
    // editors refused every edit, although save would have accepted it.
    let path = scratch_file("blank");
    std::fs::write(&path, " \n").unwrap();
    assert_eq!(Corrections::load_from(&path).unwrap().len(), 0);
    Corrections::from_editor_text("Beatrix").save_to(&path).unwrap();
    assert_eq!(Corrections::load_from(&path).unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn save_creates_a_missing_file() {
    let path = scratch_file("fresh");
    Corrections::from_editor_text("Beatrix").save_to(&path).unwrap();
    assert_eq!(Corrections::load_from(&path).unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn add_and_save_persists_immediately_and_keeps_existing_entries() {
    // Regression: the Dictionary pane's Add said "Saved" but only mutated
    // an in-memory copy, so closing the window lost the term. The Add path
    // must write the file before it confirms anything.
    let path = scratch_file("add-saves");
    std::fs::write(&path, r#"{ "_comment": "keep me", "todoist": "Todoist" }"#).unwrap();
    let mut c = Corrections::load_from(&path).unwrap();
    let outcome = c.add_and_save_to(None, "cece", "Cici", &path).unwrap();
    assert_eq!(outcome, QuickAdd::Saved(2));
    // The committed file carries the new term, the old one, and the comment.
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("keep me"), "{body}");
    let on_disk = Corrections::load_from(&path).unwrap();
    assert_eq!(on_disk.len(), 2);
    assert_eq!(on_disk.apply("cece said hi"), "Cici said hi");
    assert_eq!(on_disk.apply("todoist"), "Todoist");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn add_with_a_rename_drops_the_old_key_on_disk() {
    let path = scratch_file("add-rename");
    std::fs::write(&path, r#"{ "acmelabs": "Acme Labs" }"#).unwrap();
    let mut c = Corrections::load_from(&path).unwrap();
    assert_eq!(
        c.add_and_save_to(Some("acmelabs"), "acme labbs", "Acme Labs", &path)
            .unwrap(),
        QuickAdd::Saved(1)
    );
    let on_disk = Corrections::load_from(&path).unwrap();
    assert_eq!(
        on_disk.entries(),
        vec![("acme labbs".to_string(), "Acme Labs".to_string())]
    );
    assert_eq!(on_disk.apply("acmelabs"), "acmelabs");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn add_without_a_target_writes_nothing() {
    let path = scratch_file("add-notarget");
    std::fs::write(&path, r#"{ "todoist": "Todoist" }"#).unwrap();
    let before = std::fs::read_to_string(&path).unwrap();
    let mut c = Corrections::load_from(&path).unwrap();
    assert_eq!(
        c.add_and_save_to(None, "cece", "   ", &path).unwrap(),
        QuickAdd::NoTarget
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    assert_eq!(c.len(), 1);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn a_refused_add_leaves_the_file_and_memory_untouched() {
    // A file this build cannot parse is refused, not overwritten; the Add
    // path must not pretend the edit landed.
    let path = scratch_file("add-refused");
    let broken = "{ \"to twist\": \"Todoist\",, }";
    std::fs::write(&path, broken).unwrap();
    let mut c = Corrections::from_editor_text("Todoist");
    let err = c
        .add_and_save_to(None, "cece", "Cici", &path)
        .unwrap_err()
        .to_string();
    assert!(err.contains("could not be read"), "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
    assert_eq!(
        c.entries(),
        vec![("todoist".to_string(), "Todoist".to_string())]
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn remove_and_save_persists_immediately() {
    let path = scratch_file("remove-saves");
    std::fs::write(&path, r#"{ "todoist": "Todoist", "macos": "macOS" }"#).unwrap();
    let mut c = Corrections::load_from(&path).unwrap();
    assert!(c.remove_and_save_to("todoist", &path).unwrap());
    let on_disk = Corrections::load_from(&path).unwrap();
    assert_eq!(on_disk.len(), 1);
    assert_eq!(on_disk.apply("todoist went away"), "todoist went away");
    assert_eq!(on_disk.apply("macos"), "macOS");
    // Removing a key that is not there is a no-op, not an error.
    assert!(!c.remove_and_save_to("nothing", &path).unwrap());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn csv_import_strips_a_utf8_bom() {
    let mut c = Corrections::empty();
    let added = c.import_csv("\u{feff}from,to\nto twist,Todoist\n").unwrap();
    assert_eq!(added, 1);
    assert!(!c.to_editor_lines().iter().any(|l| l.contains('\u{feff}')));
}

fn dict(pairs: &[(&str, &str)]) -> Corrections {
    let map = pairs
        .iter()
        .map(|(k, v)| (k.to_lowercase(), v.to_string()))
        .collect();
    Corrections::build(map)
}

#[test]
fn entries_are_key_sorted_pairs() {
    let c = dict(&[("teh", "the"), ("eefa", "Aoife"), ("macos", "macOS")]);
    assert_eq!(
        c.entries(),
        vec![
            ("eefa".to_string(), "Aoife".to_string()),
            ("macos".to_string(), "macOS".to_string()),
            ("teh".to_string(), "the".to_string()),
        ]
    );
}

#[test]
fn search_matches_either_side_and_blank_matches_all() {
    let c = dict(&[("eefa", "Aoife"), ("teh", "the"), ("macos", "macOS")]);
    // The heard side.
    assert_eq!(
        c.search("eef"),
        vec![("eefa".to_string(), "Aoife".to_string())]
    );
    // The replacement side, case-insensitively.
    assert_eq!(
        c.search("MACOS"),
        vec![("macos".to_string(), "macOS".to_string())]
    );
    assert_eq!(c.search("  ").len(), 3);
    assert!(c.search("nothing here").is_empty());
}

#[test]
fn remove_drops_the_entry_and_its_phrase_index() {
    let mut c = dict(&[("to twist", "Todoist"), ("teh", "the")]);
    assert!(c.remove("TO TWIST"));
    assert_eq!(c.len(), 1);
    assert_eq!(c.apply("open to twist now"), "open to twist now");
    assert_eq!(c.apply("teh end"), "the end");
    assert!(!c.remove("to twist"));
}

#[test]
fn edit_round_trip_replaces_the_original_key() {
    // What the Dictionary pane does on a row click + Add: drop the key it
    // pulled into the fields, then re-add the edited line.
    let mut c = dict(&[("acmelabs", "Acme Labs")]);
    c.remove("acmelabs");
    let line = quick_add_line("acme labbs", "Acme Labs").unwrap();
    let mut lines = c.to_editor_lines();
    lines.push(line);
    c = Corrections::from_editor_text(&lines.join("\n"));
    assert_eq!(
        c.entries(),
        vec![("acme labbs".to_string(), "Acme Labs".to_string())]
    );
}

#[test]
fn empty_dict_returns_input_unchanged() {
    let c = Corrections::empty();
    assert_eq!(c.apply("anything goes here"), "anything goes here");
}

#[test]
fn basic_word_replacement() {
    let c = dict(&[("eefa", "Aoife")]);
    assert_eq!(
        c.apply("Hello eefa how are you"),
        "Hello Aoife how are you"
    );
}

#[test]
fn replacement_is_case_insensitive_match() {
    let c = dict(&[("eefa", "Aoife")]);
    assert_eq!(c.apply("EEFA Eefa eefa"), "Aoife Aoife Aoife");
}

#[test]
fn does_not_match_inside_other_words() {
    let c = dict(&[("eefa", "Aoife")]);
    assert_eq!(c.apply("eefaworth eefamith"), "eefaworth eefamith");
}

#[test]
fn punctuation_is_a_word_boundary() {
    let c = dict(&[("eefa", "Aoife")]);
    assert_eq!(c.apply("Hi, eefa!"), "Hi, Aoife!");
    assert_eq!(c.apply("(eefa)"), "(Aoife)");
}

#[test]
fn multiple_replacements_in_one_pass() {
    let c = dict(&[("teh", "the"), ("macos", "macOS")]);
    assert_eq!(c.apply("teh macos thing"), "the macOS thing");
}

#[test]
fn preserves_apostrophes_in_contractions() {
    let c = dict(&[("dont", "don't")]);
    // "dont" should match; "don't" itself should not become "don'\\'t"
    assert_eq!(c.apply("I dont know"), "I don't know");
    assert_eq!(c.apply("I don't know"), "I don't know");
}

#[test]
fn punctuation_only_passes_through() {
    let c = dict(&[("a", "b")]);
    assert_eq!(c.apply("... !!! ???"), "... !!! ???");
}

#[test]
fn matches_multi_word_phrase() {
    // The whole point of the rewrite: multi-word keys actually fire now.
    let c = dict(&[("to twist", "Todoist")]);
    assert_eq!(
        c.apply("send articles to twist now"),
        "send articles Todoist now"
    );
    // Case-insensitive across the whole phrase; surrounding spacing kept.
    assert_eq!(c.apply("To Twist"), "Todoist");
}

#[test]
fn hyphen_and_space_spelling_match_same_phrase_key() {
    // A key written with a hyphen matches both hyphen and space input.
    let c = dict(&[("t-twist", "Todoist")]);
    assert_eq!(c.apply("my T-Twist"), "my Todoist");
    assert_eq!(c.apply("my t twist"), "my Todoist");
}

#[test]
fn phrase_does_not_span_sentence_punctuation() {
    // "to" ends a sentence and "Twist" starts the next — not the phrase.
    let c = dict(&[("to twist", "Todoist")]);
    assert_eq!(c.apply("get to. Twist it"), "get to. Twist it");
}

#[test]
fn longest_phrase_anchored_at_a_word_wins() {
    let c = dict(&[("new york", "NYC"), ("new york city", "NYC metro")]);
    assert_eq!(c.apply("from new york city today"), "from NYC metro today");
    assert_eq!(c.apply("from new york today"), "from NYC today");
}

#[test]
fn single_word_behavior_unchanged_alongside_phrases() {
    let c = dict(&[("macos", "macOS"), ("to twist", "Todoist")]);
    assert_eq!(c.apply("ship macos to twist"), "ship macOS Todoist");
    // A phrase key does not enable partial-word matches.
    assert_eq!(c.apply("macostuff"), "macostuff");
}

#[test]
fn real_world_brand_phrasings_resolve_through_phrase_match() {
    // Parakeet mishears "Todoist" as the literal phrase "to do list" / "do
    // list"; the cleanup model then emits "to-do list" / "do list". The
    // post-cleanup correction pass must resolve both spellings (hyphen and
    // space) to the brand — a 3-token and a 2-token phrase, mid-sentence.
    let c = dict(&[("to do list", "Todoist"), ("do list", "Todoist")]);
    assert_eq!(
        c.apply("Double check the to-do list."),
        "Double check the Todoist."
    );
    assert_eq!(
        c.apply("Double check the do list."),
        "Double check the Todoist."
    );
}

#[test]
fn editor_lines_render_fixes_and_bare_words() {
    let c = dict(&[
        ("eefa", "Aoife"),
        ("github", "GitHub"),
        ("parakeet", "Parakeet"),
    ]);
    let lines = c.to_editor_lines();
    // Casing-only fixes are identical ignoring case → clean bare words.
    assert!(lines.contains(&"Parakeet".to_string()), "{lines:?}");
    assert!(lines.contains(&"GitHub".to_string()), "{lines:?}");
    // Only a genuine phonetic fix (different letters) shows "from → to".
    assert!(lines.contains(&"eefa → Aoife".to_string()), "{lines:?}");
}

#[test]
fn from_editor_text_parses_arrows_and_bare_words() {
    let c = Corrections::from_editor_text("_a comment\nfoo -> Bar\nBaz\nmacos → macOS\n  \n");
    assert_eq!(c.apply("foo"), "Bar"); // -> arrow
    assert_eq!(c.apply("macos"), "macOS"); // → arrow
    assert_eq!(c.apply("baz"), "Baz"); // bare word = identity, keeps casing
    assert_eq!(c.len(), 3); // comment + blank dropped
}

#[test]
fn quick_add_line_formats_like_editor_lines() {
    // A real mishearing fix → arrow line, keyed on what was heard.
    assert_eq!(
        quick_add_line("Julian", "Julien"),
        Some("Julian → Julien".to_string())
    );
    // Blank "heard" → a bare keep-this-spelling word.
    assert_eq!(quick_add_line("", "GitHub"), Some("GitHub".to_string()));
    // Same word, different case → identity/keep, rendered bare (no arrow).
    assert_eq!(
        quick_add_line("github", "GitHub"),
        Some("GitHub".to_string())
    );
    // No target spelling → nothing to add.
    assert_eq!(quick_add_line("Julian", "   "), None);
    // Inputs are trimmed.
    assert_eq!(
        quick_add_line("  eefa ", " Aoife "),
        Some("eefa → Aoife".to_string())
    );
}

#[test]
fn quick_add_line_round_trips_through_corrections() {
    // The line the quick-add fields produce must parse back to the intended fix.
    let line = quick_add_line("Julian", "Julien").unwrap();
    let c = Corrections::from_editor_text(&line);
    assert_eq!(c.apply("Julian"), "Julien");
}

#[test]
fn editor_round_trip_preserves_behavior() {
    let c = dict(&[("eefa", "Aoife"), ("macos", "macOS")]);
    let text = c.to_editor_lines().join("\n");
    let c2 = Corrections::from_editor_text(&text);
    assert_eq!(c2.apply("eefa"), "Aoife");
    assert_eq!(c2.apply("macos"), "macOS");
}

// --- CSV parser ---

#[test]
fn parse_csv_quoted_field_with_comma() {
    assert_eq!(parse_csv("\"a,b\",c"), vec![vec!["a,b", "c"]]);
}

#[test]
fn parse_csv_quoted_field_with_escaped_quote() {
    assert_eq!(
        parse_csv("\"say \"\"hi\"\"\",x"),
        vec![vec!["say \"hi\"", "x"]]
    );
}

#[test]
fn parse_csv_quoted_field_with_newline() {
    assert_eq!(parse_csv("\"line1\nline2\",x"), vec![vec!["line1\nline2", "x"]]);
}

#[test]
fn parse_csv_simple_unquoted_row() {
    assert_eq!(parse_csv("a,b,c"), vec![vec!["a", "b", "c"]]);
}

#[test]
fn parse_csv_trailing_empty_field() {
    assert_eq!(parse_csv("a,b,"), vec![vec!["a", "b", ""]]);
}

#[test]
fn parse_csv_trims_unquoted_fields() {
    assert_eq!(parse_csv("  a ,  b  "), vec![vec!["a", "b"]]);
}

// --- import_csv ---

#[test]
fn import_csv_two_column_substitution() {
    let mut c = Corrections::empty();
    assert_eq!(c.import_csv("eefa,Aoife").unwrap(), 1);
    assert_eq!(c.apply("hello eefa"), "hello Aoife");
}

#[test]
fn import_csv_one_column_identity() {
    let mut c = Corrections::empty();
    assert_eq!(c.import_csv("macOS").unwrap(), 1);
    assert_eq!(c.apply("macos"), "macOS");
}

#[test]
fn import_csv_skips_header_row() {
    let mut c = Corrections::empty();
    assert_eq!(c.import_csv("from,to\neefa,Aoife").unwrap(), 1);
    assert_eq!(c.len(), 1);
    assert_eq!(c.apply("eefa"), "Aoife");
}

#[test]
fn import_csv_skips_recognized_headers_case_insensitively() {
    let mut c = Corrections::empty();
    assert_eq!(c.import_csv("Heard,Correct\nfoo,Foo").unwrap(), 1);
    assert_eq!(c.apply("foo"), "Foo");
}

#[test]
fn import_csv_skips_blank_lines() {
    let mut c = Corrections::empty();
    assert_eq!(c.import_csv("\n\neefa,Aoife\n\n").unwrap(), 1);
    assert_eq!(c.len(), 1);
}

#[test]
fn import_csv_merges_without_wiping_existing() {
    let mut c = dict(&[("teh", "the")]);
    assert_eq!(c.import_csv("eefa,Aoife").unwrap(), 1);
    assert_eq!(c.len(), 2);
    assert_eq!(c.apply("teh eefa"), "the Aoife");
}

#[test]
fn import_csv_count_returns_net_new_entries() {
    let mut c = Corrections::empty();
    let n = c
        .import_csv("from,to\neefa,Aoife\nfoo,Bar\n\n")
        .unwrap();
    assert_eq!(n, 2); // header + blank skipped
    assert_eq!(c.apply("foo"), "Bar");
}

#[test]
fn import_csv_overwrites_existing_key_without_counting() {
    let mut c = dict(&[("eefa", "Aoife")]);
    // Same key, new value → 0 added (net-new only).
    assert_eq!(c.import_csv("eefa,Ling2").unwrap(), 0);
    assert_eq!(c.apply("eefa"), "Ling2");
    assert_eq!(c.len(), 1);
}

#[test]
fn import_csv_quoted_fields_with_commas() {
    let mut c = Corrections::empty();
    // A multi-word phrase key with a comma inside a quoted value, and a
    // quoted identity word preserved verbatim.
    assert_eq!(c.import_csv("\"secret phrase\",\"Secret, LLC\"").unwrap(), 1);
    assert_eq!(c.apply("the secret phrase"), "the Secret, LLC");
}
