//! Fast, model-free invariants for the software-of-one S1 evaluation corpus.

use serde::Deserialize;
use std::collections::HashSet;

#[derive(Deserialize)]
struct Case {
    id: String,
    category: String,
    input: String,
    #[serde(default)]
    must_contain: Vec<String>,
    #[serde(default)]
    must_not_contain: Vec<String>,
    #[serde(default)]
    preserve_exact: Vec<String>,
    #[serde(default)]
    known_vocabulary: Vec<String>,
    #[serde(default)]
    min_char_retention_ratio: Option<f64>,
    #[serde(default)]
    max_expansion_ratio: Option<f64>,
}

#[test]
fn s1_corpus_is_nonempty_unique_and_well_formed() {
    let cases: Vec<Case> =
        serde_json::from_str(include_str!("../evals/s1_mini_corpus.json")).unwrap();
    assert!(cases.len() >= 10);
    let mut ids = HashSet::new();
    for case in cases {
        assert!(ids.insert(case.id), "duplicate case id");
        assert!(!case.category.trim().is_empty());
        assert!(!case.input.trim().is_empty());
        assert!(case.must_contain.iter().all(|s| !s.trim().is_empty()));
        assert!(case.must_not_contain.iter().all(|s| !s.trim().is_empty()));
        assert!(case.preserve_exact.iter().all(|s| !s.trim().is_empty()));
        assert!(case.known_vocabulary.iter().all(|s| !s.trim().is_empty()));
        let min_char_retention = case.min_char_retention_ratio.unwrap_or(0.0);
        assert!((0.0..=1.0).contains(&min_char_retention));
        assert!(case.max_expansion_ratio.unwrap_or(4.0) > 1.0);
    }
}

#[test]
fn dictionary_cases_have_hints_for_exact_spellings() {
    let cases: Vec<Case> =
        serde_json::from_str(include_str!("../evals/s1_mini_corpus.json")).unwrap();
    for case in cases.iter().filter(|c| c.category == "dictionary") {
        assert!(
            !case.preserve_exact.is_empty(),
            "{} needs an exact vocabulary assertion",
            case.id
        );
        assert!(
            !case.known_vocabulary.is_empty(),
            "{} needs a vocabulary hint",
            case.id
        );
    }
}
