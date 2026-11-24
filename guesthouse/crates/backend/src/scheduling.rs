//! Host-requested scheduling controls. Defaults remain unsupported on other backends.

use crate::BackendError;
use std::num::NonZeroU64;

#[derive(Debug)]
pub struct GraphicsTimesliceState {
    /// None means no explicit request for future groups; it is not a hardware default readback.
    pub requested_micros: Option<NonZeroU64>,
    pub graphics_queues: usize,
}

#[derive(Debug)]
#[must_use = "inspect all queue results: scheduling changes can partially fail"]
pub struct GraphicsTimesliceUpdate {
    pub requested_micros: NonZeroU64,
    pub queues: Vec<(u32, Result<(), BackendError>)>,
}

/// NVIDIA 615 ctrla06c.h interleave levels. These are requests, not fixed GPU shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum GraphicsInterleaveLevel {
    Low = 0,
    Medium = 1,
    High = 2,
}

impl GraphicsInterleaveLevel {
    pub fn from_raw(value: u32) -> Option<Self> {
        match value { 0 => Some(Self::Low), 1 => Some(Self::Medium), 2 => Some(Self::High), _ => None }
    }
}

#[derive(Debug)]
pub struct GraphicsInterleaveState {
    /// None means no request; Low (zero) is a real request, not a reset to a default.
    pub requested_level: Option<GraphicsInterleaveLevel>,
    pub graphics_queues: usize,
}

#[derive(Debug)]
#[must_use = "inspect all queue results: scheduling changes can partially fail"]
pub struct GraphicsInterleaveUpdate {
    pub requested_level: GraphicsInterleaveLevel,
    pub queues: Vec<(u32, Result<(), BackendError>)>,
}
