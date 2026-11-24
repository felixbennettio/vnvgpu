//! Host-side explicit RM object charges, not physical residency or hidden driver memory.
use std::num::NonZeroU64;

/// The configured limit and conservative object charges, all in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoBudgetState {
    pub limit: Option<NonZeroU64>,
    /// Live RM objects plus freed-but-exported objects. Saturates only for an impossible-to-report
    /// total above u64::MAX; admission itself compares the full u128 sum.
    pub charged_bytes: u64,
    pub live_object_bytes: u64,
    pub retained_export_bytes: u64,
    /// Subset of live_object_bytes whose over-limit allocation rollback was refused by RM.
    pub failed_rollback_bytes: u64,
}

impl VideoBudgetState {
    /// Explicitly charged and still admissible bytes in this virtual budget.
    pub fn allocated_and_available(&self) -> Option<(u64, u64)> {
        self.limit.map(|limit| (self.charged_bytes, limit.get().saturating_sub(self.charged_bytes)))
    }

    /// A finite reservation for this snapshot, or unknown for an uncapped backend.
    /// Never lend charged objects back simply because the configured cap was lowered.
    pub fn admission_reservation_bytes(&self) -> Option<u64> {
        self.limit.map(|limit| limit.get().max(self.charged_bytes))
    }
}

#[cfg(test)]
mod budget_usage_tests {
    use super::*;

    #[test]
    fn available_capacity_respects_retained_exports_and_lowered_limits() {
        let mut state = VideoBudgetState {
            limit: NonZeroU64::new(1024), charged_bytes: 768, live_object_bytes: 512,
            retained_export_bytes: 256, failed_rollback_bytes: 0,
        };
        assert_eq!(state.allocated_and_available(), Some((768, 256)));
        state.limit = NonZeroU64::new(768);
        assert_eq!(state.allocated_and_available(), Some((768, 0)));
        state.limit = NonZeroU64::new(512);
        assert_eq!(state.allocated_and_available(), Some((768, 0)));
        state.limit = None;
        assert_eq!(state.allocated_and_available(), None);
    }
}
