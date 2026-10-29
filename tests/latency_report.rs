//! Runs `scripts/latency-report.sh` on a synthetic log (invented numbers, no text).
use std::process::Command;

fn script() -> String {
    format!("{}/scripts/latency-report.sh", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn reports_percentiles_and_uninserted_share() {
    let dir = std::env::temp_dir().join(format!("ld-latency-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("daemon.log");
    let mut text = String::from("⏹ stopped · held 1.00s\nunrelated line\n");
    for i in 1..=10u32 {
        let ins = if i == 10 { "false" } else { "true" };
        text += &format!("[latency] release_ms={} tail_ms=0 capture_stop_ms=1 asr_ms={} cleanup_ms={} focus_wait_ms=0 insertion_return_ms=1 inserted={}\n", i * 100, i * 10, i, ins);
    }
    std::fs::write(&log, text).unwrap();
    let out = Command::new("bash").arg(script()).arg(&log).output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines[0], "n=10");
    assert_eq!(lines[1], "release_ms n=10 p50=500 p90=900 p99=1000");
    assert_eq!(lines[2], "asr_ms n=10 p50=50 p90=90 p99=100");
    assert_eq!(lines[3], "cleanup_ms n=10 p50=5 p90=9 p99=10");
    assert_eq!(lines[4], "inserted_false=10.0%");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn missing_file_fails_with_one_line() {
    let out = Command::new("bash")
        .arg(script())
        .arg("/nonexistent/ld-daemon.log")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stderr).lines().count(), 1);
}

fn run_on(name: &str, body: &str) -> std::process::Output {
    let dir = std::env::temp_dir().join(format!("ld-latency-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("daemon.log");
    std::fs::write(&log, body).unwrap();
    let out = Command::new("bash").arg(script()).arg(&log).output().unwrap();
    std::fs::remove_dir_all(&dir).ok();
    out
}

#[test]
fn a_key_missing_from_every_line_reports_n0() {
    let out = run_on(
        "nokey",
        "[latency] release_ms=100 tail_ms=0 asr_ms=10 inserted=true\n[latency] release_ms=200 tail_ms=0 asr_ms=20 inserted=true\n",
    );
    assert!(out.status.success());
    let s = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines[1], "release_ms n=2 p50=100 p90=200 p99=200");
    assert_eq!(lines[3], "cleanup_ms n=0");
}

#[test]
fn a_log_without_latency_lines_fails_with_one_line() {
    let out = run_on("empty", "unrelated line\n");
    assert!(!out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stderr).lines().count(), 1);
}
