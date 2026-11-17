//! When to reuse, start or restart a warm engine. A pure function of what is known about
//! the running engine, so every rule is tested without a process.
//!
//! A warm engine's peak RSS only ever goes up: it levels off at the largest chunk it has
//! served, a little above a cold process. So before every chunk (and every warm-up) the
//! engine is restarted when its peak so far plus a measured per-chunk headroom would
//! cross the budget. The budget therefore holds for every chunk, not just on average.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WarmPolicy {
    /// Ceiling on the engine's peak RSS, in KiB.
    pub budget_kib: u64,
    /// The most one chunk may add to a warm engine's peak RSS, in KiB.
    pub headroom_kib: u64,
}

/// What is known about the running engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineView<'a> {
    /// The reference of the voice it serves.
    pub voice: &'a Path,
    /// Its peak RSS so far, in KiB.
    pub peak_rss_kib: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Use the running engine.
    Reuse,
    /// No engine is running: start one.
    Start,
    /// Stop the running engine and start a fresh one.
    Restart(RestartReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartReason {
    /// The next chunk could take its peak RSS over the budget.
    NearBudget { peak_rss_kib: u64 },
    /// It serves another voice (one engine holds one voice's conditioning).
    VoiceChanged,
}

impl std::fmt::Display for RestartReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestartReason::NearBudget { peak_rss_kib } => {
                write!(
                    f,
                    "peak RSS {peak_rss_kib} KiB is within the restart headroom"
                )
            }
            RestartReason::VoiceChanged => f.write_str("another voice"),
        }
    }
}

impl WarmPolicy {
    /// The peak RSS above which an engine must not take another chunk.
    pub fn restart_above_kib(&self) -> u64 {
        self.budget_kib.saturating_sub(self.headroom_kib)
    }

    pub fn plan(&self, engine: Option<EngineView<'_>>, voice: &Path) -> Plan {
        let Some(e) = engine else {
            return Plan::Start;
        };
        if e.voice != voice {
            return Plan::Restart(RestartReason::VoiceChanged);
        }
        if e.peak_rss_kib > self.restart_above_kib() {
            return Plan::Restart(RestartReason::NearBudget {
                peak_rss_kib: e.peak_rss_kib,
            });
        }
        Plan::Reuse
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: WarmPolicy = WarmPolicy {
        budget_kib: 1_100_000,
        headroom_kib: 100_000,
    };

    fn view(voice: &str, peak_rss_kib: u64) -> EngineView<'_> {
        EngineView {
            voice: Path::new(voice),
            peak_rss_kib,
        }
    }

    #[test]
    fn no_engine_starts_one() {
        assert_eq!(POLICY.plan(None, Path::new("/v/a.wav")), Plan::Start);
    }

    #[test]
    fn a_warm_engine_for_the_same_voice_is_reused_up_to_the_threshold() {
        let a = Path::new("/v/a.wav");
        assert_eq!(POLICY.restart_above_kib(), 1_000_000);
        // Measured: 0.916 GB after the warm-up, 0.946 GB after a short chunk.
        assert_eq!(POLICY.plan(Some(view("/v/a.wav", 916_000)), a), Plan::Reuse);
        assert_eq!(POLICY.plan(Some(view("/v/a.wav", 946_000)), a), Plan::Reuse);
        assert_eq!(
            POLICY.plan(Some(view("/v/a.wav", 1_000_000)), a),
            Plan::Reuse
        );
    }

    #[test]
    fn an_engine_within_the_headroom_of_the_budget_is_restarted() {
        let a = Path::new("/v/a.wav");
        // Measured: 1.021 GB after a 19-word chunk; one more could reach 1.1 GB.
        assert_eq!(
            POLICY.plan(Some(view("/v/a.wav", 1_000_001)), a),
            Plan::Restart(RestartReason::NearBudget {
                peak_rss_kib: 1_000_001
            })
        );
        assert_eq!(
            POLICY.plan(Some(view("/v/a.wav", 1_200_000)), a),
            Plan::Restart(RestartReason::NearBudget {
                peak_rss_kib: 1_200_000
            })
        );
    }

    #[test]
    fn another_voice_restarts_even_a_fresh_engine() {
        assert_eq!(
            POLICY.plan(Some(view("/v/a.wav", 200_000)), Path::new("/v/b.wav")),
            Plan::Restart(RestartReason::VoiceChanged)
        );
    }

    #[test]
    fn a_headroom_at_least_the_budget_restarts_before_every_chunk() {
        let p = WarmPolicy {
            budget_kib: 100,
            headroom_kib: 200,
        };
        assert_eq!(p.restart_above_kib(), 0);
        assert!(matches!(
            p.plan(Some(view("/a", 1)), Path::new("/a")),
            Plan::Restart(RestartReason::NearBudget { .. })
        ));
    }
}
