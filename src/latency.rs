//! Privacy-safe release-pipeline timings. These measure application work, not
//! when another application's pixels become visible after a paste request.
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Snapshot {
    pub release_ms: u64,
    pub tail_ms: u64,
    pub capture_stop_ms: u64,
    pub asr_ms: u64,
    pub cleanup_ms: u64,
    pub focus_ms: u64,
    pub insertion_ms: u64,
    pub inserted: bool,
}

impl Snapshot {
    pub fn menu_label(self) -> String {
        if self.inserted {
            format!(
                "Last insertion request: {} ms after release",
                self.release_ms
            )
        } else {
            format!("Last result held: {} ms after release", self.release_ms)
        }
    }

    pub fn log_line(self) -> String {
        format!("[latency] release_ms={} tail_ms={} capture_stop_ms={} asr_ms={} cleanup_ms={} focus_wait_ms={} insertion_return_ms={} inserted={}", self.release_ms, self.tail_ms, self.capture_stop_ms, self.asr_ms, self.cleanup_ms, self.focus_ms, self.insertion_ms, self.inserted)
    }
}

static LAST: OnceLock<Mutex<Option<Snapshot>>> = OnceLock::new();

pub fn record(snapshot: Snapshot) {
    if let Ok(mut slot) = LAST.get_or_init(|| Mutex::new(None)).lock() {
        *slot = Some(snapshot);
    }
    eprintln!("{}", snapshot.log_line());
}

pub fn last() -> Option<Snapshot> {
    LAST.get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|slot| *slot)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timings_name_their_boundary_and_never_claim_visible_pixels() {
        let measured = Snapshot {
            release_ms: 614,
            tail_ms: 250,
            capture_stop_ms: 4,
            asr_ms: 180,
            cleanup_ms: 40,
            focus_ms: 20,
            insertion_ms: 120,
            inserted: true,
        };
        assert_eq!(
            measured.menu_label(),
            "Last insertion request: 614 ms after release"
        );
        assert_eq!(measured.log_line(), "[latency] release_ms=614 tail_ms=250 capture_stop_ms=4 asr_ms=180 cleanup_ms=40 focus_wait_ms=20 insertion_return_ms=120 inserted=true");
        assert_eq!(
            Snapshot {
                inserted: false,
                ..measured
            }
            .menu_label(),
            "Last result held: 614 ms after release"
        );
    }
}
