use super::*;

#[test]
fn short_audio_is_one_whole_range() {
    let audio = vec![0.5_f32; SAMPLE_RATE as usize * 5];
    assert_eq!(split_for_asr(&audio, SAMPLE_RATE, 30.0), vec![(0, audio.len())]);
}

#[test]
fn empty_audio_yields_one_empty_range() {
    assert_eq!(split_for_asr(&[], SAMPLE_RATE, 30.0), vec![(0, 0)]);
}

#[test]
fn long_audio_is_split_under_the_cap_with_full_coverage() {
    let sr = SAMPLE_RATE;
    let audio = vec![0.5_f32; sr as usize * 100];
    let segs = split_for_asr(&audio, sr, 30.0);
    assert!(segs.len() >= 4, "100 s at a 30 s cap needs ≥4 chunks: {segs:?}");
    let max_len = (30.0 * sr as f32) as usize;
    for &(s, e) in &segs {
        assert!(e > s, "empty range in {segs:?}");
        assert!(e - s <= max_len, "chunk over the cap in {segs:?}");
    }
    // Gapless, non-overlapping, complete coverage.
    assert_eq!(segs[0].0, 0);
    assert_eq!(segs.last().unwrap().1, audio.len());
    for w in segs.windows(2) {
        assert_eq!(w[0].1, w[1].0, "gap or overlap in {segs:?}");
    }
}

#[test]
fn cut_lands_in_the_quiet_gap() {
    let sr = SAMPLE_RATE;
    let mut audio = vec![0.5_f32; sr as usize * 40];
    // A 400 ms silence at 25 s, inside the 18–30 s search window.
    let gap_start = sr as usize * 25;
    let gap_end = gap_start + sr as usize * 4 / 10;
    for s in &mut audio[gap_start..gap_end] {
        *s = 0.0;
    }
    let segs = split_for_asr(&audio, sr, 30.0);
    let cut = segs[0].1;
    assert!(
        cut >= gap_start && cut <= gap_end,
        "cut {cut} outside the silence {gap_start}..{gap_end}"
    );
}
