//! Charges for this RM client's explicit local-memory objects.
//!
//! A successful RM free is sufficient for an unexported object. An exported dma-buf can outlive
//! that free, and this layer cannot observe all consumers closing their copies, so its charge
//! remains until the Device ends. No eviction, hidden RM allocations, or residency is claimed.

use super::{Handle, RmError};
use std::collections::HashMap;
use std::num::NonZeroU64;

pub use guesthouse_backend::video_budget::VideoBudgetState;

#[derive(Debug)]
struct Charge {
    bytes: u64,
    live: bool,
    exported: bool,
    rollback_failed: bool,
}

#[derive(Debug, Default)]
pub(super) struct VideoBudget {
    limit: Option<NonZeroU64>,
    objects: HashMap<Handle, Charge>,
}

fn reported(bytes: u128) -> u64 {
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

impl VideoBudget {
    pub fn set_limit(&mut self, bytes: NonZeroU64) {
        self.limit = Some(bytes);
    }

    fn charged(&self) -> u128 {
        self.objects.values().map(|charge| u128::from(charge.bytes)).sum()
    }

    pub fn admit(&self, bytes: u64) -> Result<(), RmError> {
        if let Some(limit) = self.limit {
            let charged = self.charged();
            if charged + u128::from(bytes) > u128::from(limit.get()) {
                return Err(RmError::VideoMemoryBudget {
                    charged, requested: bytes, limit: limit.get(),
                });
            }
        }
        Ok(())
    }

    pub fn allocated(&mut self, handle: Handle, bytes: u64) {
        // Device supplies monotonically increasing handles, so a live object is never replaced.
        self.objects.insert(handle, Charge { bytes, live: true, exported: false, rollback_failed: false });
    }

    pub fn rollback_failed(&mut self, handle: Handle) {
        if let Some(charge) = self.objects.get_mut(&handle) {
            charge.rollback_failed = true;
        }
    }

    pub fn exported(&mut self, handle: Handle) {
        if let Some(charge) = self.objects.get_mut(&handle) {
            charge.exported = true;
        }
    }

    pub fn freed(&mut self, handle: Handle) {
        if let Some(charge) = self.objects.get_mut(&handle) {
            if charge.exported {
                charge.live = false;
                return;
            }
        }
        self.objects.remove(&handle);
    }

    pub fn state(&self) -> VideoBudgetState {
        let sum = |predicate: fn(&Charge) -> bool| {
            reported(self.objects.values().filter(|c| predicate(c)).map(|c| u128::from(c.bytes)).sum())
        };
        VideoBudgetState {
            limit: self.limit,
            charged_bytes: reported(self.charged()),
            live_object_bytes: sum(|c| c.live),
            retained_export_bytes: sum(|c| !c.live && c.exported),
            failed_rollback_bytes: sum(|c| c.live && c.rollback_failed),
        }
    }
}
