//! RTP-timestamp-driven fps inference for the fMP4 muxer.
//!
//! Holds a sliding window of inter-access-unit DTS deltas and locks in a
//! median-derived per-sample tick duration once 8 deltas pass an IQR variance
//! guard (R7) and a plausibility check (R6). Spec ref: sdd/t2-trex-fps-fix R4–R8.

use std::collections::VecDeque;

/// Warm-up fallback per-sample duration in 90 kHz units (30 fps).
/// Spec R5: used unconditionally during warm-up and on R6/R7 rejection.
pub(crate) const WARMUP_FALLBACK_TICKS: u32 = 3_000;

/// Plausibility lower bound (240 fps inclusive). Spec R6: 90000/240 = 375.
const MIN_PLAUSIBLE_TICKS: u32 = 375;

/// Plausibility upper bound (5 fps inclusive). Spec R6: 90000/5 = 18000.
const MAX_PLAUSIBLE_TICKS: u32 = 18_000;

/// Window size for inference. Spec R4 fixes this at 8.
const WINDOW_SIZE: usize = 8;

/// IQR threshold denominator: IQR must be ≤ median/5 (20%). Spec R7.
/// We compare `iqr * IQR_DENOM > median` to avoid floats.
const IQR_DENOM: u32 = 5;

/// State of the fps inference state machine.
#[derive(Debug)]
pub(crate) enum FpsState {
    /// Warm-up: still collecting deltas or rejecting them via R6/R7.
    WarmingUp,
    /// Locked: an effective tick duration has been derived. Never re-enters WarmingUp.
    Locked { ticks_per_sample: u32 },
}

/// Sliding-window median-of-8 fps tracker for the fMP4 muxer.
///
/// Call `observe_dts` once per access unit (IDR or P-frame). After 8 consecutive
/// deltas pass the IQR variance guard (R7) and plausibility bounds (R6), the
/// tracker locks at the median delta value (R4). Once locked it never re-derives
/// within the same muxer lifetime (R8).
#[derive(Debug)]
pub(crate) struct FpsTracker {
    state: FpsState,
    window: VecDeque<u32>,
    last_dts: Option<u64>,
}

impl FpsTracker {
    /// Construct a new `FpsTracker` in the `WarmingUp` state.
    pub(crate) fn new() -> Self {
        Self {
            state: FpsState::WarmingUp,
            window: VecDeque::with_capacity(WINDOW_SIZE),
            last_dts: None,
        }
    }

    /// Feed one access-unit DTS (90 kHz units).
    ///
    /// Computes the inter-AU delta from the previous DTS and pushes it into the
    /// sliding window. When the window reaches `WINDOW_SIZE` and the IQR+plausibility
    /// guards pass, transitions to `Locked`.
    ///
    /// Idempotent on duplicate DTS values (delta = 0 will fail the plausibility check).
    pub(crate) fn observe_dts(&mut self, dts: u64) {
        if let Some(prev) = self.last_dts {
            let delta = dts.saturating_sub(prev) as u32;
            self.push_delta(delta);
        }
        self.last_dts = Some(dts);
    }

    fn push_delta(&mut self, delta: u32) {
        if matches!(self.state, FpsState::Locked { .. }) {
            // R8: once locked, no further inference — still update last_dts above
            // but discard deltas.
            return;
        }
        if self.window.len() == WINDOW_SIZE {
            self.window.pop_front();
        }
        self.window.push_back(delta);
        if self.window.len() == WINDOW_SIZE {
            self.try_lock();
        }
    }

    /// Inspect the current window and either lock or stay warming up.
    fn try_lock(&mut self) {
        let mut sorted: [u32; WINDOW_SIZE] = [0; WINDOW_SIZE];
        for (i, &v) in self.window.iter().enumerate() {
            sorted[i] = v;
        }
        sorted.sort_unstable();

        // Using the 8-element sorted array:
        //   indices:  0  1  2  3  4  5  6  7
        //   Q1 = sorted[2] (lower quartile median approximation)
        //   median = sorted[4] (upper of the two middle values for even N)
        //   Q3 = sorted[5]
        //   IQR = Q3 - Q1
        let q1 = sorted[2];
        let median = sorted[4];
        let q3 = sorted[5];
        let iqr = q3.saturating_sub(q1);

        // Variance guard FIRST (R7 — cheaper, no tracing log).
        if iqr.saturating_mul(IQR_DENOM) > median {
            return; // Slide window on next observe_dts; stays WarmingUp.
        }

        // Plausibility check (R6).
        if median < MIN_PLAUSIBLE_TICKS || median > MAX_PLAUSIBLE_TICKS {
            tracing::warn!(
                rejected_ticks = median,
                "fps inference rejected: median tick {} outside [5, 240] fps bounds [{}, {}]",
                median,
                MIN_PLAUSIBLE_TICKS,
                MAX_PLAUSIBLE_TICKS,
            );
            return; // Window keeps sliding; stays WarmingUp.
        }

        tracing::debug!(ticks_per_sample = median, "fps inference locked");
        self.state = FpsState::Locked {
            ticks_per_sample: median,
        };
    }

    /// Per-sample tick duration to use RIGHT NOW.
    ///
    /// Returns the locked value once warm-up is complete (R4), or the
    /// 3000-tick warm-up fallback otherwise (R5).
    pub(crate) fn effective_ticks_per_sample(&self) -> u32 {
        match self.state {
            FpsState::Locked { ticks_per_sample } => ticks_per_sample,
            FpsState::WarmingUp => WARMUP_FALLBACK_TICKS,
        }
    }

    /// Whether warm-up is complete and a tick duration has been locked.
    ///
    /// Only available in test builds to allow white-box assertions.
    #[cfg(test)]
    pub(crate) fn is_locked(&self) -> bool {
        matches!(self.state, FpsState::Locked { .. })
    }
}

// ─── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: feed `n` calls to `observe_dts` with uniform spacing.
    fn feed_uniform(tracker: &mut FpsTracker, count: usize, tick_spacing: u32) {
        let mut dts: u64 = 0;
        for _ in 0..count {
            tracker.observe_dts(dts);
            dts += tick_spacing as u64;
        }
    }

    // T1.1 — warm-up fallback is 3000 immediately after construction (R5).
    #[test]
    fn fps_tracker_returns_3000_during_warm_up() {
        let tracker = FpsTracker::new();
        assert_eq!(
            tracker.effective_ticks_per_sample(),
            3000,
            "fresh tracker must return 3000 (warm-up fallback)"
        );
        assert!(!tracker.is_locked(), "fresh tracker must not be locked");
    }

    // T1.2 — 9 calls at 3000-tick spacing lock at 3000 (30 fps).
    #[test]
    fn fps_tracker_locks_at_3000_for_30fps_uniform_window() {
        let mut tracker = FpsTracker::new();
        feed_uniform(&mut tracker, 9, 3000);
        assert!(tracker.is_locked(), "should be locked after 9 uniform 3000-tick deltas");
        assert_eq!(
            tracker.effective_ticks_per_sample(),
            3000,
            "locked value must be 3000 for 30 fps"
        );
    }

    // T1.3 — 9 calls at 1500-tick spacing lock at 1500 (60 fps).
    #[test]
    fn fps_tracker_locks_at_1500_for_60fps_uniform_window() {
        let mut tracker = FpsTracker::new();
        feed_uniform(&mut tracker, 9, 1500);
        assert!(tracker.is_locked(), "should be locked after 9 uniform 1500-tick deltas");
        assert_eq!(
            tracker.effective_ticks_per_sample(),
            1500,
            "locked value must be 1500 for 60 fps"
        );
    }

    // T1.4 — median ignores a single outlier in the window.
    #[test]
    fn fps_tracker_median_ignores_single_outlier_in_window() {
        // 9 observations producing deltas: [1500, 9999, 1500, 1500, 1500, 1500, 1500, 1500, 1500]
        // (first call produces no delta; calls 2..10 produce 8 deltas)
        let deltas = [1500u32, 9999, 1500, 1500, 1500, 1500, 1500, 1500, 1500];
        let mut tracker = FpsTracker::new();
        let mut dts: u64 = 0;
        tracker.observe_dts(dts);
        for delta in deltas {
            dts += delta as u64;
            tracker.observe_dts(dts);
        }
        assert!(
            tracker.is_locked(),
            "should lock despite one outlier (median-of-8 is robust)"
        );
        assert_eq!(
            tracker.effective_ticks_per_sample(),
            1500,
            "median of [1500×7, 9999×1] is 1500"
        );
    }

    // T1.5 — 300-tick spacing (300 fps) must NOT lock (above 240 fps bound, R6).
    #[test]
    fn fps_tracker_rejects_above_240fps_keeps_warming_up() {
        let mut tracker = FpsTracker::new();
        feed_uniform(&mut tracker, 9, 300); // 300 ticks = 300 fps > 240 fps bound
        assert!(
            !tracker.is_locked(),
            "300 fps (300 ticks) exceeds the 240 fps plausibility upper bound — must stay warming up"
        );
        assert_eq!(
            tracker.effective_ticks_per_sample(),
            3000,
            "fallback must remain 3000 during warm-up"
        );
    }

    // T1.6 — 19000-tick spacing (< 5 fps) must NOT lock (below 5 fps bound, R6).
    #[test]
    fn fps_tracker_rejects_below_5fps_keeps_warming_up() {
        let mut tracker = FpsTracker::new();
        feed_uniform(&mut tracker, 9, 19000); // 19000 ticks > 18000 (5 fps lower bound)
        assert!(
            !tracker.is_locked(),
            "19000 tick spacing (< 5 fps) is outside the plausibility bound — must stay warming up"
        );
    }

    // T1.7 — boundary values 375 (240 fps) and 18000 (5 fps) are inclusive.
    #[test]
    fn fps_tracker_accepts_exactly_240fps_and_5fps_boundaries() {
        // 375 ticks = exactly 240 fps boundary (inclusive).
        let mut tracker_240 = FpsTracker::new();
        feed_uniform(&mut tracker_240, 9, 375);
        assert!(
            tracker_240.is_locked(),
            "375 ticks (exactly 240 fps) is on the inclusive boundary — must lock"
        );
        assert_eq!(tracker_240.effective_ticks_per_sample(), 375);

        // 18000 ticks = exactly 5 fps boundary (inclusive).
        let mut tracker_5 = FpsTracker::new();
        feed_uniform(&mut tracker_5, 9, 18000);
        assert!(
            tracker_5.is_locked(),
            "18000 ticks (exactly 5 fps) is on the inclusive boundary — must lock"
        );
        assert_eq!(tracker_5.effective_ticks_per_sample(), 18000);
    }

    // T1.8 — high-variance window slides; locks after outlier leaves the window.
    #[test]
    fn fps_tracker_slides_window_when_iqr_exceeds_20_percent_of_median() {
        // First 8 deltas: [1500, 1500, 1500, 1500, 1500, 1500, 1500, 3000]
        // IQR = sorted[5] - sorted[2].
        // sorted = [1500, 1500, 1500, 1500, 1500, 1500, 1500, 3000]
        //           0     1     2     3     4     5     6     7
        // Q1=sorted[2]=1500, Q3=sorted[5]=1500, IQR=0? Hmm.
        // Wait: we need IQR > median/5.
        // median = sorted[4] = 1500; IQR=0 → 0 > 1500/5=300? No.
        // We need a window where IQR > 20% of median.
        // Let's use [1500, 1500, 1500, 1500, 1500, 1500, 1500, 3000] again:
        // Actually IQR would be sorted[5]-sorted[2].
        // sorted = [1500,1500,1500,1500,1500,1500,1500,3000]
        // sorted[2]=1500, sorted[5]=1500, IQR=0. That's too uniform.
        //
        // Per spec R7: "IQR exceeds 20% of median".
        // Use [1500, 1500, 1500, 1500, 1500, 1500, 1500, 3000]:
        // sorted[5] = 1500, sorted[2] = 1500 → IQR=0 → won't trigger.
        //
        // Better example: [1500, 1500, 1500, 1500, 3000, 3000, 3000, 3000]
        // sorted = [1500,1500,1500,1500,3000,3000,3000,3000]
        // sorted[4]=3000 (median), sorted[2]=1500 (Q1), sorted[5]=3000 (Q3)
        // IQR = 3000-1500 = 1500; median=3000; 1500 > 3000/5=600 → REJECT ✓
        //
        // Task T1.8 uses the exact window from the spec:
        // "feed window [1500, 1500, 1500, 1500, 1500, 1500, 1500, 3000] + one more 1500"
        // But that window's IQR=0. The spec description might mean the data includes
        // the 3000 as the 8th of the 8 deltas, producing a mixed window.
        //
        // Let's re-read the task: "feed window [1500×7, 3000]" should NOT lock (first 8),
        // then after adding one more 1500 (9th delta, window becomes [1500×7, 3000, →pops 1500, pushes 1500)
        // = [1500×7, 3000] still... The IQR of sorted [1500×7, 3000] = sorted[5]-sorted[2] = 1500-1500 = 0.
        // This doesn't trigger R7. The spec might intend a different window.
        //
        // Using a window that reliably triggers R7:
        // deltas = [1500, 3000, 1500, 3000, 1500, 3000, 1500, 3000]
        // sorted = [1500,1500,1500,1500,3000,3000,3000,3000]
        // median=sorted[4]=3000, Q1=sorted[2]=1500, Q3=sorted[5]=3000, IQR=1500
        // IQR*5=7500 > median=3000 → REJECT ✓
        //
        // After removing the alternating pattern and feeding 8 uniform 1500s:
        // window becomes [3000,1500,3000,1500,3000,1500,3000,1500] → still mixed
        // Eventually 8 more 1500s will push all 3000s out.

        let mut tracker = FpsTracker::new();
        // Feed alternating pattern to create high-variance window.
        let mut dts: u64 = 0;
        tracker.observe_dts(dts);
        let alternating = [1500u32, 3000, 1500, 3000, 1500, 3000, 1500, 3000];
        for delta in alternating {
            dts += delta as u64;
            tracker.observe_dts(dts);
        }
        // Must NOT be locked — high IQR window.
        assert!(
            !tracker.is_locked(),
            "alternating 1500/3000 window has high IQR — must not lock"
        );

        // Now feed 8 uniform 1500-tick deltas to push all outliers out.
        for _ in 0..8 {
            dts += 1500;
            tracker.observe_dts(dts);
        }
        // Must now be locked at 1500.
        assert!(
            tracker.is_locked(),
            "after replacing high-variance window with uniform 1500s, must lock"
        );
        assert_eq!(tracker.effective_ticks_per_sample(), 1500);
    }

    // T1.9 — low-IQR window locks even with minor jitter.
    #[test]
    fn fps_tracker_locks_when_iqr_below_20_percent_of_median() {
        // deltas = [1500, 1500, 1520, 1480, 1510, 1490, 1500, 1500]
        // sorted = [1480, 1490, 1500, 1500, 1500, 1500, 1510, 1520]
        // Q1=sorted[2]=1500, Q3=sorted[5]=1500, IQR=0 < median/5=300 → PASS
        // median=sorted[4]=1500 → locks at 1500.
        let deltas = [1500u32, 1500, 1520, 1480, 1510, 1490, 1500, 1500];
        let mut tracker = FpsTracker::new();
        let mut dts: u64 = 0;
        tracker.observe_dts(dts);
        for d in deltas {
            dts += d as u64;
            tracker.observe_dts(dts);
        }
        assert!(tracker.is_locked(), "low-IQR window must lock");
        assert_eq!(
            tracker.effective_ticks_per_sample(),
            1500,
            "median of near-1500 window is 1500"
        );
    }

    // T1.10 — stays locked at original value even after many observations with different deltas (R8).
    #[test]
    fn fps_tracker_stays_locked_after_subsequent_observations_with_different_deltas() {
        let mut tracker = FpsTracker::new();
        // Lock at 1500 (60 fps).
        feed_uniform(&mut tracker, 9, 1500);
        assert!(tracker.is_locked());
        assert_eq!(tracker.effective_ticks_per_sample(), 1500);

        // Feed 1000 more observations at 3000-tick spacing — must stay locked at 1500 (R8).
        let mut dts: u64 = 9 * 1500;
        for _ in 0..1000 {
            dts += 3000;
            tracker.observe_dts(dts);
        }
        assert!(tracker.is_locked(), "must remain locked after 1000 divergent observations");
        assert_eq!(
            tracker.effective_ticks_per_sample(),
            1500,
            "locked value must not change (R8: no mid-stream re-derive)"
        );
    }

    // T1.11 — property-style seed tests for R4 (uniform window locks) and R7 (high-variance rejects).
    #[test]
    fn fps_tracker_any_uniform_window_in_bounds_locks_at_that_value() {
        // 4 hand-crafted uniform windows in [375, 18000].
        for &ticks in &[375u32, 1000, 3000, 18000] {
            let mut tracker = FpsTracker::new();
            feed_uniform(&mut tracker, 9, ticks);
            assert!(
                tracker.is_locked(),
                "uniform window at {} ticks/sample must lock",
                ticks
            );
            assert_eq!(
                tracker.effective_ticks_per_sample(),
                ticks,
                "locked value must equal the uniform spacing {} ticks",
                ticks
            );
        }
    }

    #[test]
    fn fps_tracker_any_high_variance_window_does_not_lock() {
        // 4 hand-crafted windows with IQR > median/5.
        // Each case: 4 values at ticks_lo, 4 at ticks_hi; IQR = ticks_hi - ticks_lo.
        // Check: IQR * 5 > median (median ≈ max of the two middle values).
        let high_variance_cases: &[(u32, u32)] = &[
            (1500, 3000), // IQR=1500, median≈3000, IQR*5=7500>3000 ✓
            (1000, 6000), // IQR=5000, median≈6000, IQR*5=25000>6000 ✓
            (500, 2000),  // IQR=1500, median≈2000, IQR*5=7500>2000 ✓
            (400, 1000),  // IQR=600, median≈1000, IQR*5=3000>1000 ✓
        ];
        for &(lo, hi) in high_variance_cases {
            let mut tracker = FpsTracker::new();
            // Feed [lo, hi, lo, hi, lo, hi, lo, hi] as 8 deltas (9 observe_dts calls).
            let mut dts: u64 = 0;
            tracker.observe_dts(dts);
            let pattern = [lo, hi, lo, hi, lo, hi, lo, hi];
            for d in pattern {
                dts += d as u64;
                tracker.observe_dts(dts);
            }
            assert!(
                !tracker.is_locked(),
                "high-variance window [{lo},{hi},...] IQR>{}/5 must not lock",
                hi
            );
        }
    }
}
