use super::cleanup_ran_away;
use super::{
    plan_reuse, prompt_len_problem, raw_is_filler_only, reuse_after_clear,
    transform_input_problem, MAX_CTX, MAX_TRANSFORM_CHARS,
};

#[test]
fn hybrid_partial_clear_refusal_resets_the_whole_cache() {
    // Normal memory: the divergent tail is cleared, the prefix is reused.
    assert_eq!(reuse_after_clear(true, 7), (7, false));
    // Hybrid/recurrent memory refused the partial removal: reuse nothing and
    // reset fully, rather than trust state the cache no longer holds.
    assert_eq!(reuse_after_clear(false, 7), (0, true));
    assert_eq!(reuse_after_clear(false, 0), (0, true));
}

#[test]
fn filler_only_input_may_normalize_to_empty() {
    assert!(raw_is_filler_only("um"));
    assert!(raw_is_filler_only("  Um, uh... hmm "));
    assert!(!raw_is_filler_only("um, ship it"));
    assert!(!raw_is_filler_only(""));
    assert!(!raw_is_filler_only("I mean it"));
}

#[test]
fn reuse_never_exceeds_what_a_failed_job_left_in_the_cache() {
    // Job A ran: its prompt is resident.
    let mut resident = vec![1, 2, 3, 4, 5, 6, 7, 8];
    // Job B shares 3 tokens, wipes the cache from 3, then FAILS.
    let job_b = [1, 2, 3, 40, 50, 60];
    let reuse_b = plan_reuse(&job_b, &resident, false);
    assert_eq!(reuse_b, 3);
    resident.truncate(reuse_b); // what run_job now does right after the wipe
    // Job C is job A's prompt again. Before the fix `resident` still held
    // all 8 tokens, so C "reused" 7 that the cache no longer contained.
    let job_c = [1, 2, 3, 4, 5, 6, 7, 8];
    assert_eq!(plan_reuse(&job_c, &resident, false), 3);
}

#[test]
fn reuse_always_leaves_one_token_to_decode() {
    let resident = vec![1, 2, 3];
    assert_eq!(plan_reuse(&[1, 2, 3], &resident, false), 2);
    assert_eq!(plan_reuse(&[1, 2, 3, 4], &resident, false), 3);
    assert_eq!(plan_reuse(&[9], &resident, false), 0);
    assert_eq!(plan_reuse(&[1, 2, 3], &resident, true), 0);
    assert_eq!(plan_reuse::<i32>(&[], &resident, false), 0);
}

#[test]
fn oversize_prompts_are_rejected_before_the_cache_is_touched() {
    assert!(prompt_len_problem(400).is_none());
    assert!(prompt_len_problem(MAX_CTX as usize - 64).is_none());
    assert!(prompt_len_problem(MAX_CTX as usize - 63).is_some());
    assert!(prompt_len_problem(20_000).is_some());
}

#[test]
fn transform_refuses_a_selection_it_could_only_truncate() {
    assert!(transform_input_problem("short selection").is_none());
    assert!(transform_input_problem(&"a".repeat(MAX_TRANSFORM_CHARS)).is_none());
    let msg = transform_input_problem(&"é".repeat(MAX_TRANSFORM_CHARS + 1)).unwrap();
    assert!(msg.contains("left unchanged"), "{msg}");
}

#[test]
fn runaway_guard() {
    // Normal cleanup (slightly shorter/longer) is fine.
    assert!(!cleanup_ran_away(
        "um so i think we should ship it",
        "I think we should ship it."
    ));
    assert!(!cleanup_ran_away("call it parakeet", "Call it Parakeet."));
    // Empty input → never a runaway.
    assert!(!cleanup_ran_away("", ""));
    // Dropped all content from real input → runaway.
    assert!(cleanup_ran_away("ship the build today", ""));
    // Ballooned far beyond input → runaway.
    let raw = "ship it";
    let huge = "Here is a long elaboration ".repeat(20);
    assert!(cleanup_ran_away(raw, &huge));
    // Leaked chat-template marker → runaway even if length is reasonable.
    assert!(cleanup_ran_away(
        "ship it now please",
        "Ship it now please.<end_of_turn>"
    ));
}

#[test]
fn repeated_token_run_is_runaway() {
    assert!(cleanup_ran_away(
        "send the draft tomorrow",
        "users users users users users users"
    ));
}
