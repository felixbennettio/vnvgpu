//! The card's memory as `NV2080_CTRL_CMD_FB_GET_INFO_V2` tells it, under the guest's video budget.
//!
//! The RM answers this control for the whole card. Windows' `nvidia-smi` takes its total from the
//! guest's segments, which already show the budget, and its free memory from this answer's
//! `HEAP_FREE`, so with a 3 GiB budget on an 8 GiB card it showed a negative "used" (measured,
//! RTX 3070: 3072 MiB total, 2702 free, used 2^44 MiB - 2701). The answer is rewritten so the guest
//! sees its budget as the card, as the Linux path's device does: sizes capped at the budget, the
//! heap keeping the host's reserve (`TOTAL_RAM_SIZE` - `HEAP_SIZE`) below it, and the free heap what
//! the budget leaves after this guest's allocations. Nothing is raised above what the RM said.

use super::{get, put};
use guesthouse_backend::VideoMemoryUse;

pub(super) const CONTROL: u32 = 0x2080_1303;
const MAX_LIST: u32 = 128;
const RAM_SIZE: u32 = 0x07;
const TOTAL_RAM_SIZE: u32 = 0x08;
const HEAP_SIZE: u32 = 0x09;
const MAPPABLE_HEAP_SIZE: u32 = 0x0A;
const LARGEST_FREE_REGION: u32 = 0x12;
const HEAP_FREE: u32 = 0x16;
const USABLE_RAM_SIZE: u32 = 0x20;

const LIST: usize = 4;
const ENTRY_SIZE: usize = 8;
const KIB: u64 = 1024;

fn entry(index: u32) -> usize {
    LIST + index as usize * ENTRY_SIZE
}

/// Rewrites an answered list in `params` (all values in KiB) for `memory`.
pub(super) fn budget(memory: VideoMemoryUse, params: &mut [u8]) {
    let Some(count) = get(params, 0) else { return };
    if count > MAX_LIST || params.len() < entry(count) {
        return;
    }
    let value_of = |params: &[u8], wanted: u32| {
        (0..count).find_map(|i| (get(params, entry(i))? == wanted).then(|| get(params, entry(i) + 4)).flatten())
    };
    let limit = memory.total / KIB;
    let used = memory.used.div_ceil(KIB);
    let (total, heap) = (value_of(params, TOTAL_RAM_SIZE), value_of(params, HEAP_SIZE));
    let mut heap_now = match (total, heap) {
        (Some(total), Some(heap)) if heap > 0 && heap <= total => limit.saturating_sub(u64::from(total - heap)),
        _ => limit,
    };
    if let Some(heap) = heap {
        heap_now = heap_now.min(u64::from(heap));
    }
    let free = heap_now.saturating_sub(used);
    for i in 0..count {
        let at = entry(i);
        let (Some(index), Some(value)) = (get(params, at), get(params, at + 4)) else { return };
        let cap = match index {
            RAM_SIZE | TOTAL_RAM_SIZE | USABLE_RAM_SIZE => limit,
            HEAP_SIZE | MAPPABLE_HEAP_SIZE => heap_now,
            HEAP_FREE | LARGEST_FREE_REGION => free,
            _ => continue,
        };
        put(params, at + 4, value.min(u32::try_from(cap).unwrap_or(u32::MAX)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB_KIB: u32 = 1 << 20;

    fn list(entries: &[(u32, u32)]) -> Vec<u8> {
        let mut params = vec![0; entry(entries.len() as u32)];
        put(&mut params, 0, entries.len() as u32);
        for (i, &(index, value)) in entries.iter().enumerate() {
            put(&mut params, entry(i as u32), index);
            put(&mut params, entry(i as u32) + 4, value);
        }
        params
    }

    #[test]
    fn a_whole_card_answer_becomes_the_budget_with_the_host_reserve_kept() {
        // 8 GiB card, 300 MiB reserved, 5.6 GiB free on the card; the guest: 3 GiB, 370 MiB used.
        let reserve = 300 << 10;
        let mut params = list(&[
            (TOTAL_RAM_SIZE, 8 * GIB_KIB),
            (HEAP_SIZE, 8 * GIB_KIB - reserve),
            (HEAP_FREE, 5773 << 10),
            (0x01, 77),
        ]);
        budget(VideoMemoryUse { total: 3 << 30, used: 370 << 20 }, &mut params);
        assert_eq!(get(&params, entry(0) + 4), Some(3 * GIB_KIB));
        assert_eq!(get(&params, entry(1) + 4), Some(3 * GIB_KIB - reserve));
        assert_eq!(get(&params, entry(2) + 4), Some(3 * GIB_KIB - reserve - (370 << 10)));
        assert_eq!(get(&params, entry(3) + 4), Some(77), "other entries stay");
    }

    #[test]
    fn nothing_is_raised_and_a_bad_list_is_left_alone() {
        let mut params = list(&[(HEAP_FREE, 100)]);
        budget(VideoMemoryUse { total: 3 << 30, used: 0 }, &mut params);
        assert_eq!(get(&params, entry(0) + 4), Some(100));
        let mut short = list(&[(HEAP_FREE, 100)]);
        put(&mut short, 0, 5);
        let before = short.clone();
        budget(VideoMemoryUse { total: 3 << 30, used: 0 }, &mut short);
        assert_eq!(short, before);
    }
}
