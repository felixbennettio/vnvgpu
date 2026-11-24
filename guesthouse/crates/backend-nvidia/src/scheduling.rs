//! Runtime requests for this backend's graphics channel groups.
//!
//! These calls report RM acceptance, not hardware readback or a proportional scheduling guarantee.

use crate::NvidiaBackend;
use crate::queue::QueueChannel;
use crate::rm::{Rm, RmError};
use std::num::NonZeroU64;

/// Every extant graphics queue is attempted, even after another queue fails. Successful requests
/// are not rolled back: rollback can itself fail and the RM's default is not known here.
#[derive(Debug)]
#[must_use = "inspect each queue result; a timeslice update can partially fail"]
pub struct TimesliceUpdate {
    pub requested_micros: NonZeroU64,
    /// Sorted guest queue IDs and the actual result returned by each owned group's RM control.
    /// Copy/video channels and queues with no channel are omitted.
    pub queues: Vec<(u32, Result<(), RmError>)>,
}

impl<R: Rm> NvidiaBackend<R> {
    pub(crate) fn requested_graphics_timeslice(
        &self,
    ) -> guesthouse_backend::scheduling::GraphicsTimesliceState {
        let graphics_queues = self.queues.values().filter(|queue| match &queue.channel {
            QueueChannel::Unusable => false,
            QueueChannel::Ring { channel, .. } => channel.is_graphics(),
            QueueChannel::List(list) => list.channel.is_graphics(),
        }).count();
        guesthouse_backend::scheduling::GraphicsTimesliceState {
            requested_micros: NonZeroU64::new(self.device.graphics_timeslice_micros),
            graphics_queues,
        }
    }


    /// Updates existing graphics groups and the requested slice for future graphics channels.
    ///
    /// The desired future value is saved even on partial failure, so retrying this operation
    /// converges all surviving groups on the same request. Future channel creation retains the
    /// existing warning on RM refusal. This report covers only the groups present in this call.
    /// The exclusive backend borrow serializes this with queue creation/destruction/submission.
    ///
    /// Zero is deliberately excluded: the old startup setter's zero means "do not send a control",
    /// not "restore the hardware default". This API neither invents that default nor claims an
    /// atomic transaction across groups. Success means the RM accepted the request; verify any
    /// throughput, latency or fairness claim separately on hardware.
    pub fn update_graphics_timeslice(&mut self, micros: NonZeroU64) -> TimesliceUpdate {
        self.device.graphics_timeslice_micros = micros.get();
        let mut ids: Vec<_> = self.queues.keys().copied().collect();
        ids.sort_unstable();
        let mut queues = Vec::new();
        for id in ids {
            let channel = match &self.queues[&id].channel {
                QueueChannel::Unusable => continue,
                QueueChannel::Ring { channel, .. } => channel,
                QueueChannel::List(list) => &list.channel,
            };
            if let Some(result) = channel.update_graphics_timeslice(&mut self.device, micros) {
                queues.push((id, result));
            }
        }
        TimesliceUpdate { requested_micros: micros, queues }
    }
}

#[derive(Debug)]
#[must_use = "inspect each queue result; an interleave update can partially fail"]
pub struct InterleaveUpdate {
    pub requested_level: guesthouse_backend::scheduling::GraphicsInterleaveLevel,
    pub queues: Vec<(u32, Result<(), RmError>)>,
}

impl<R: Rm> NvidiaBackend<R> {
    pub(crate) fn requested_graphics_interleave(
        &self,
    ) -> guesthouse_backend::scheduling::GraphicsInterleaveState {
        guesthouse_backend::scheduling::GraphicsInterleaveState {
            requested_level: self.device.graphics_interleave_level,
            graphics_queues: self.requested_graphics_timeslice().graphics_queues,
        }
    }

    /// Requests interleave for every extant graphics TSG and saves desired policy for future
    /// graphics channels, even on refusal. Exclusive borrow serializes queue lifecycle changes.
    /// Each RM result is preserved; no rollback, privilege acquisition, hardware readback or
    /// proportional share is implied. Future channel refusals remain warnings, like timeslice.
    pub fn update_graphics_interleave(
        &mut self,
        level: guesthouse_backend::scheduling::GraphicsInterleaveLevel,
    ) -> InterleaveUpdate {
        self.device.graphics_interleave_level = Some(level);
        let mut ids: Vec<_> = self.queues.keys().copied().collect();
        ids.sort_unstable();
        let mut queues = Vec::new();
        for id in ids {
            let channel = match &self.queues[&id].channel {
                QueueChannel::Unusable => continue,
                QueueChannel::Ring { channel, .. } => channel,
                QueueChannel::List(list) => &list.channel,
            };
            if let Some(result) = channel.update_graphics_interleave(&mut self.device, level) {
                queues.push((id, result));
            }
        }
        InterleaveUpdate { requested_level: level, queues }
    }
}
