//! The objects this backend keeps in the RM: one client, one device, and what hangs from them.

use super::classes::{
    CONTROL_GET_GPU_ID, CONTROL_GET_TIME, DEVICE, MEMORY_LOCAL_USER, MEMORY_VIRTUAL, SUBDEVICE,
    VIRTUAL_ADDRESS_SPACE,
};
use super::instance::device_instance_at;
use super::listing::CardPlace;
use super::{DmaMap, Handle, Rm, RmError};
use crate::guest_clock::GuestClock;
use crate::profile::HostBranch;
use guesthouse_backend::{MemoryKind, PciAddress};
use std::os::fd::OwnedFd;
use std::collections::HashMap;
use std::num::NonZeroU64;
use super::video_budget::{VideoBudget, VideoBudgetState};

const DEVICE_HANDLE: Handle = 0xC000_0001;
const SUBDEVICE_HANDLE: Handle = 0xC000_0002;
const FIRST_OBJECT: Handle = 0xC000_1000;

/// Which of the host's GPUs the device is: the RM's device instance, which isn't the device file's
/// number. See [`super::instance`].
const DEVICE_PARAM_INSTANCE: usize = 0x00;
/// Each channel gets its own address space from this client, which needs this mode on the device.
const VA_MODE_MULTIPLE: u32 = 2;
const DEVICE_PARAM_VA_MODE: usize = 0x30;

/// A tag the RM keeps with memory. Any value but zero does.
const MEMORY_OWNER: u32 = 0x6770_7576;
const MEMORY_PARAM_OWNER: usize = 0x00;
const MEMORY_PARAM_TYPE: usize = 0x04;
const MEMORY_PARAM_FLAGS: usize = 0x08;
const MEMORY_PARAM_ATTRIBUTES: usize = 0x18;
const MEMORY_PARAM_SIZE: usize = 0x40;
const MEMORY_PARAM_OFFSET: usize = 0x50;
const MEMORY_PARAM_ADDRESS_SPACE: usize = 0x6C;
const MEMORY_FLAG_HANDLE_PROVIDED: u32 = 0x4000;
/// Memory for a depth surface, which the RM gives the depth kind its attributes' packing calls for.
const MEMORY_TYPE_DEPTH: u32 = 1;
/// Virtual, at a fixed address, with the handle provided, and lazily. Without lazily the page tables
/// for the whole range are made up front, which takes seconds, and every process needs its own.
const VIRTUAL_RANGE_FLAGS: u32 = 0x0008_0000 | 0x0000_0010 | 0x0000_4000 | 0x0000_0400;
const ADDRESS_SPACE_PARAM_SIZE: usize = 0x08;
const ADDRESS_SPACE_SIZE: u64 = 1 << 40;
/// The kernel takes a size that isn't whole pages for a bug of its own.
const EXPORT_GRAIN: u64 = 0x1000;

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// A GPU address space, and the ranges reserved in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressSpace {
    pub handle: Handle,
    /// (start, end, the range's object).
    pub ranges: Vec<(u64, u64, Handle)>,
}

impl AddressSpace {
    pub fn range_at(&self, address: u64) -> Result<Handle, RmError> {
        let found = self.ranges.iter().find(|(start, end, _)| (*start..*end).contains(&address));
        found.map(|range| range.2).ok_or_else(|| RmError::System(format!("no range holds {address:#x}")))
    }
}

/// A mapping to make, at a fixed address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mapping {
    pub memory: Handle,
    pub offset: u64,
    pub length: u64,
    pub address: u64,
    pub small_pages: bool,
    pub gpu_cached: bool,
}

/// The client, and the handles it hands out.
pub struct Device<R> {
    pub rm: R,
    /// How long a graphics channel group may hold the GPU. 0 leaves the RM's own choice.
    pub graphics_timeslice_micros: u64,
    /// Desired policy only. None leaves RM alone; Low is an explicit request.
    pub graphics_interleave_level: Option<guesthouse_backend::scheduling::GraphicsInterleaveLevel>,
    pub client: Handle,
    pub device: Handle,
    pub subdevice: Handle,
    /// The RM's device instance of this GPU.
    pub instance: u32,
    /// One doorbell page serves every channel.
    pub doorbell_mapped: bool,
    pub(crate) guest_clock: GuestClock,
    next_handle: Handle,
    /// RM allocation results belong to this client, whose handles are only locally unique.
    /// Insert after allocation succeeds; retain on failed free; drop with this device/session.
    memory_kinds: HashMap<Handle, u32>,
    video_budget: VideoBudget,
    sizes: &'static HostBranch,
}

impl<R: Rm> Device<R> {
    /// Makes the client, device and subdevice that everything else hangs from, on the RM's first
    /// device. That's only sure to be the card whose device file is open on a PC with one card.
    pub fn open(mut rm: R, sizes: &'static HostBranch) -> Result<Self, RmError> {
        let client = rm.new_client()?;
        Self::open_instance(rm, sizes, client, 0)
    }

    /// The same on the GPU at this address, whose device file the RM must have open.
    pub fn open_at(mut rm: R, sizes: &'static HostBranch, address: &PciAddress) -> Result<Self, RmError> {
        let client = rm.new_client()?;
        let instance = device_instance_at(&mut rm, client, address)?;
        Self::open_instance(rm, sizes, client, instance)
    }

    /// By its address where that's known, and otherwise as [`Device::open`] does.
    pub fn open_place(rm: R, sizes: &'static HostBranch, place: &CardPlace) -> Result<Self, RmError> {
        match &place.address {
            Some(address) => Self::open_at(rm, sizes, address),
            None => Self::open(rm, sizes),
        }
    }

    fn open_instance(
        mut rm: R,
        sizes: &'static HostBranch,
        client: Handle,
        instance: u32,
    ) -> Result<Self, RmError> {
        let mut params = vec![0; sizes.device_params];
        put_u32(&mut params, DEVICE_PARAM_INSTANCE, instance);
        put_u32(&mut params, DEVICE_PARAM_VA_MODE, VA_MODE_MULTIPLE);
        let status = rm.alloc(client, DEVICE_HANDLE, DEVICE, &mut params)?;
        RmError::check(|| format!("class {DEVICE:#x}"), status)?;
        let mut params = vec![0; sizes.subdevice_params];
        let status = rm.alloc(DEVICE_HANDLE, SUBDEVICE_HANDLE, SUBDEVICE, &mut params)?;
        RmError::check(|| format!("class {SUBDEVICE:#x}"), status)?;
        Ok(Self {
            graphics_timeslice_micros: 0,
            graphics_interleave_level: None,
            rm,
            client,
            device: DEVICE_HANDLE,
            subdevice: SUBDEVICE_HANDLE,
            instance,
            doorbell_mapped: false,
            guest_clock: GuestClock::default(),
            next_handle: FIRST_OBJECT,
            memory_kinds: HashMap::new(),
            video_budget: VideoBudget::default(),
            sizes,
        })
    }

    pub fn new_handle(&mut self) -> Handle {
        let handle = self.next_handle;
        self.next_handle += 1;
        handle
    }

    /// Makes an object, and treats any status but success as failure.
    pub fn alloc(&mut self, parent: Handle, class: u32, params: &mut [u8]) -> Result<Handle, RmError> {
        let video_size = if class == MEMORY_LOCAL_USER {
            let bytes = params.get(MEMORY_PARAM_SIZE..MEMORY_PARAM_SIZE + 8)
                .ok_or_else(|| RmError::System("video allocation has no size".into()))?;
            let size = u64::from_le_bytes(bytes.try_into().expect("eight bytes"));
            self.video_budget.admit(size)?;
            true
        } else {
            false
        };
        let handle = self.new_handle();
        let status = self.rm.alloc(parent, handle, class, params)?;
        RmError::check(|| format!("class {class:#x}"), status)?;
        if video_size {
            // The RM can round the requested size. Charge its answer, not a guessed page size.
            let bytes = &params[MEMORY_PARAM_SIZE..MEMORY_PARAM_SIZE + 8];
            let given = u64::from_le_bytes(bytes.try_into().expect("eight bytes"));
            let over = self.video_budget.admit(given).err();
            self.video_budget.allocated(handle, given);
            if let Some(error) = over {
                // It has not been returned to a caller, mapped, or exported. If rollback fails,
                // keep the real object charged and name its handle; never manufacture a free.
                if let Err(free_error) = self.free(handle) {
                    self.video_budget.rollback_failed(handle);
                    return Err(RmError::System(format!(
                        "{error}; rollback of {handle:#x} failed: {free_error}; allocation remains charged"
                    )));
                }
                return Err(error);
            }
        }
        Ok(handle)
    }

    /// Runs a control, and treats any status but success as failure.
    pub fn control(&mut self, object: Handle, command: u32, params: &mut [u8]) -> Result<(), RmError> {
        let status = self.rm.control(object, command, params)?;
        RmError::check(|| format!("control {command:#x}"), status)
    }

    /// The size of a control's parameters, if a guest may run it.
    pub fn guest_control_size(&self, command: u32) -> Option<usize> {
        let found = self.sizes.guest_controls.iter().find(|control| control.command == command);
        found.map(|control| control.params_size)
    }

    /// The object a control belongs on, from the class in the top half of its number.
    pub fn control_target(&self, command: u32) -> Handle {
        match command >> 16 {
            0 => self.client,
            0x80 => self.device,
            _ => self.subdevice,
        }
    }

    /// Makes video memory. Returns it, and the size the RM gave it.
    /// Makes video memory. A kind that isn't plain holds the attributes of a depth surface.
    pub fn alloc_video_memory(&mut self, size: u64, kind: MemoryKind) -> Result<(Handle, u64), RmError> {
        let mut params = vec![0; self.sizes.memory_params];
        put_u32(&mut params, MEMORY_PARAM_OWNER, MEMORY_OWNER);
        put_u32(&mut params, MEMORY_PARAM_FLAGS, MEMORY_FLAG_HANDLE_PROVIDED);
        put_u64(&mut params, MEMORY_PARAM_SIZE, size);
        if kind != MemoryKind::PLAIN {
            put_u32(&mut params, MEMORY_PARAM_TYPE, MEMORY_TYPE_DEPTH);
            put_u32(&mut params, MEMORY_PARAM_ATTRIBUTES, kind.0);
        }
        if crate::trace_depth() { log::info!("P1_RM_REQUEST size={size:#x} kind={:#x} params={params:02x?}", kind.0); }
        let handle = self.alloc(self.device, MEMORY_LOCAL_USER, &mut params)?;
        if crate::trace_depth() { log::info!("P1_RM_REPLY handle={handle:#x} params={params:02x?}"); }
        let given = params[MEMORY_PARAM_SIZE..MEMORY_PARAM_SIZE + 8].try_into().expect("eight bytes");
        // NV_MEMORY_ALLOCATION_PARAMS.format at +0x20 is the resolved allocation kind.
        // Preserve the actual RM result; a later physical-attribute control needs privileges.
        let physical_kind = u32::from_le_bytes(params[0x20..0x24].try_into().expect("four bytes"));
        self.memory_kinds.insert(handle, physical_kind);
        Ok((handle, u64::from_le_bytes(given)))
    }

    /// Applies to future explicit video-memory allocations; existing allocations are kept.
    /// Lowering below the charge blocks allocations until enough non-exported objects are freed.
    pub fn set_video_memory_limit(&mut self, bytes: NonZeroU64) -> VideoBudgetState {
        self.video_budget.set_limit(bytes);
        self.video_memory_budget()
    }

    /// This client's explicit RM video objects, including internal channel allocations.
    /// Exported objects remain conservatively charged after RM free until this device ends.
    /// This is not a measurement of physical residency or the driver's hidden allocations.
    pub fn video_memory_budget(&self) -> VideoBudgetState {
        self.video_budget.state()
    }

    /// The actual kind returned when this video allocation was created.
    pub fn physical_memory_kind(&self, memory: Handle) -> Option<u32> {
        self.memory_kinds.get(&memory).copied()
    }

    pub fn alloc_host_pages(&mut self, address: u64, size: u64) -> Result<Handle, RmError> {
        let handle = self.new_handle();
        self.rm.alloc_host_pages(self.device, handle, address, size)?;
        Ok(handle)
    }

    /// Makes a GPU address space covering these ranges. A single range is limited, so there are two.
    pub fn create_address_space(&mut self, ranges: &[(u64, u64)]) -> Result<AddressSpace, RmError> {
        let handle = self.new_handle();
        let range = self.new_handle();
        let mut params = vec![0; self.sizes.address_space_params];
        put_u64(&mut params, ADDRESS_SPACE_PARAM_SIZE, ADDRESS_SPACE_SIZE);
        let status = self.rm.alloc(self.device, handle, VIRTUAL_ADDRESS_SPACE, &mut params)?;
        RmError::check(|| "GPU address space".to_owned(), status)?;
        let mut space = AddressSpace { handle, ranges: Vec::with_capacity(ranges.len()) };
        for (index, &(start, end)) in ranges.iter().enumerate() {
            let object = if index == 0 { range } else { self.new_handle() };
            self.alloc_range(object, handle, start, end - start)?;
            space.ranges.push((start, end, object));
        }
        Ok(space)
    }

    fn alloc_range(&mut self, range: Handle, space: Handle, base: u64, size: u64) -> Result<(), RmError> {
        let mut params = vec![0; self.sizes.memory_params];
        put_u32(&mut params, MEMORY_PARAM_OWNER, MEMORY_OWNER);
        put_u32(&mut params, MEMORY_PARAM_FLAGS, VIRTUAL_RANGE_FLAGS);
        put_u64(&mut params, MEMORY_PARAM_SIZE, size);
        put_u64(&mut params, MEMORY_PARAM_OFFSET, base);
        put_u32(&mut params, MEMORY_PARAM_ADDRESS_SPACE, space);
        let status = self.rm.alloc(self.device, range, MEMORY_VIRTUAL, &mut params)?;
        RmError::check(|| format!("virtual range at {base:#x}"), status)
    }

    pub fn destroy_address_space(&mut self, space: &AddressSpace) -> Result<(), RmError> {
        for &(_, _, range) in &space.ranges {
            self.rm.free(self.device, range)?;
        }
        self.rm.free(self.device, space.handle)
    }

    pub fn map(&mut self, space: &AddressSpace, map: &Mapping) -> Result<(), RmError> {
        self.map_with_kind(space, map, None)
    }

    pub fn map_with_kind(&mut self, space: &AddressSpace, map: &Mapping, kind: Option<u32>) -> Result<(), RmError> {
        let range = space.range_at(map.address)?;
        self.rm.map_dma_with_kind(&DmaMap {
            device: self.device,
            range,
            memory: map.memory,
            offset: map.offset,
            length: map.length,
            address: map.address,
            small_pages: map.small_pages,
            gpu_cached: map.gpu_cached,
        }, kind)
    }

    pub fn unmap(
        &mut self,
        space: &AddressSpace,
        memory: Handle,
        address: u64,
        length: u64,
    ) -> Result<(), RmError> {
        let range = space.range_at(address)?;
        self.rm.unmap_dma(self.device, range, memory, address, length)
    }

    pub fn free(&mut self, handle: Handle) -> Result<(), RmError> {
        self.rm.free(self.device, handle)?;
        self.memory_kinds.remove(&handle);
        self.video_budget.freed(handle);
        Ok(())
    }

    /// Hands out video memory as a dmabuf, for a consumer on this card to read where it lies.
    pub fn export_dmabuf(&mut self, memory: Handle, size: u64) -> Result<OwnedFd, RmError> {
        if !size.is_multiple_of(EXPORT_GRAIN) {
            return Err(RmError::System(format!("{size:#x} bytes is not whole pages")));
        }
        let fd = self.rm.export_dmabuf(self.device, memory, size)?;
        self.video_budget.exported(memory);
        Ok(fd)
    }

    /// The GPU's clock in nanoseconds, which is what its timestamps count.
    pub fn gpu_time(&mut self) -> Result<u64, RmError> {
        let mut params = [0; 8];
        self.control(self.subdevice, CONTROL_GET_TIME, &mut params)?;
        Ok(u64::from_le_bytes(params))
    }

    /// The RM's number for this card.
    pub fn gpu_id(&mut self) -> Result<u32, RmError> {
        let mut params = [0; 4];
        self.control(self.subdevice, CONTROL_GET_GPU_ID, &mut params)?;
        Ok(u32::from_le_bytes(params))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::HOST_615;
    use crate::rm::fake::{Call, FakeGpu, FakeRm};

    fn device() -> Device<FakeRm> {
        Device::open(FakeRm::new(), &HOST_615).unwrap()
    }

    #[test]
    fn opening_makes_the_device_and_subdevice_in_order() {
        let device = device();
        let classes: Vec<u32> = device.rm.allocated().map(|(class, _)| class).collect();
        assert_eq!(classes, [DEVICE, SUBDEVICE]);
    }

    #[test]
    fn the_device_asks_for_several_address_spaces() {
        let device = device();
        let (_, params) = device.rm.allocated().next().unwrap();
        assert_eq!(params[DEVICE_PARAM_VA_MODE], VA_MODE_MULTIPLE as u8);
        assert_eq!(params.len(), HOST_615.device_params);
    }

    fn instance_in(params: &[u8]) -> u32 {
        u32::from_le_bytes(params[DEVICE_PARAM_INSTANCE..DEVICE_PARAM_INSTANCE + 4].try_into().unwrap())
    }

    /// Two cards whose device files are in the other order from the RM's instances: the 4080's file is
    /// 0 and its instance 1, the 1660 SUPER's file 1 and its instance 0.
    fn two_cards() -> Vec<FakeGpu> {
        let at = |text: &str| PciAddress::parse(text).unwrap();
        vec![
            FakeGpu { id: 0x100, address: at("0000:01:00.0"), minor: 0, device_instance: 1, attached: true },
            FakeGpu { id: 0x200, address: at("0000:02:00.0"), minor: 1, device_instance: 0, attached: false },
        ]
    }

    #[test]
    fn a_one_card_pc_opens_instance_0_either_way() {
        let device = device();
        assert_eq!(instance_in(device.rm.allocated().next().unwrap().1), 0);
        let address = PciAddress::parse("0000:01:00.0").unwrap();
        let device = Device::open_at(FakeRm::new(), &HOST_615, &address).unwrap();
        assert_eq!(device.instance, 0);
        assert_eq!(instance_in(device.rm.allocated().next().unwrap().1), 0);
    }

    #[test]
    fn a_card_is_opened_by_the_instance_the_rm_gave_its_address() {
        for (minor, address, instance) in [(1, "0000:02:00.0", 0), (0, "0000:01:00.0", 1)] {
            let mut rm = FakeRm::with_gpus(two_cards());
            rm.open_device_file(minor);
            let place = CardPlace { minor, address: PciAddress::parse(address) };
            let device = Device::open_place(rm, &HOST_615, &place).unwrap();
            assert_eq!(device.instance, instance, "{address}");
            assert_eq!(instance_in(device.rm.allocated().next().unwrap().1), instance);
        }
    }

    #[test]
    fn the_device_file_number_as_the_instance_is_what_the_rm_refused() {
        let mut rm = FakeRm::with_gpus(two_cards());
        rm.open_device_file(1);
        let error = Device::open_instance(rm, &HOST_615, 1, 1).err().unwrap();
        assert!(error.to_string().contains("status 0x1b"), "{error}");
    }

    #[test]
    fn an_address_with_no_gpu_is_said_plainly() {
        let mut rm = FakeRm::with_gpus(two_cards());
        rm.open_device_file(1);
        let error =
            Device::open_at(rm, &HOST_615, &PciAddress::parse("0000:03:00.0").unwrap()).err().unwrap();
        assert!(error.to_string().contains("no GPU set up at 0000:03:00.0"), "{error}");
    }

    #[test]
    fn video_memory_reports_what_the_rm_gave() {
        let mut device = device();
        device.rm.round_memory_to = 0x10000;
        assert_eq!(device.alloc_video_memory(0x1234, MemoryKind::PLAIN).unwrap().1, 0x10000);
    }

    fn memory_params(device: &Device<FakeRm>) -> Vec<u8> {
        let (_, params) =
            device.rm.allocated().filter(|(class, _)| *class == MEMORY_LOCAL_USER).last().unwrap();
        params.to_vec()
    }

    fn u32_in(params: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(params[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn physical_kind_keeps_each_rm_allocation_result_until_its_release() {
        let mut device = device();
        device.rm.allocated_memory_kind = 4;
        let (first, _) = device.alloc_video_memory(0x10000, MemoryKind::PLAIN).unwrap();
        device.rm.allocated_memory_kind = 1;
        let (second, _) = device.alloc_video_memory(0x10000, MemoryKind::PLAIN).unwrap();
        assert_eq!(device.physical_memory_kind(first), Some(4));
        assert_eq!(device.physical_memory_kind(second), Some(1));
        assert_eq!(device.physical_memory_kind(7), None);
        device.free(first).unwrap();
        assert_eq!(device.physical_memory_kind(first), None);
        assert_eq!(device.physical_memory_kind(second), Some(1));
        assert!(!device.rm.calls.iter().any(|c| matches!(c, Call::Control { command: 0x0041_0103, .. })));
    }

    #[test]
    fn physical_kinds_are_isolated_even_when_clients_reuse_numeric_handles() {
        let mut first = device();
        let mut second = device();
        first.rm.allocated_memory_kind = 4;
        second.rm.allocated_memory_kind = 1;
        let (a, _) = first.alloc_video_memory(0x10000, MemoryKind::PLAIN).unwrap();
        let (b, _) = second.alloc_video_memory(0x10000, MemoryKind::PLAIN).unwrap();
        assert_eq!(a, b, "independent RM clients may reuse the same object number");
        assert_eq!(first.physical_memory_kind(a), Some(4));
        assert_eq!(second.physical_memory_kind(b), Some(1));
        first.free(a).unwrap();
        assert_eq!(first.physical_memory_kind(a), None);
        assert_eq!(second.physical_memory_kind(b), Some(1));

        // A session owns its backend/device; dropping it also drops any unreleased metadata.
        drop(second);
        let mut next = device();
        assert_eq!(next.physical_memory_kind(b), None);
        next.rm.allocated_memory_kind = 6;
        let (c, _) = next.alloc_video_memory(0x10000, MemoryKind::PLAIN).unwrap();
        assert_eq!(b, c);
        assert_eq!(next.physical_memory_kind(c), Some(6));
    }

    #[test]
    fn failed_allocations_never_publish_physical_kinds() {
        let mut device = device();
        device.rm.allocated_memory_kind = 4;
        let (live, _) = device.alloc_video_memory(0x10000, MemoryKind::PLAIN).unwrap();
        for ioctl_failure in [false, true] {
            device.rm.refuse_class = (!ioctl_failure).then_some(MEMORY_LOCAL_USER);
            device.rm.fail_alloc_class = ioctl_failure.then_some(MEMORY_LOCAL_USER);
            let failed = device.next_handle;
            assert!(device.alloc_video_memory(0x10000, MemoryKind::PLAIN).is_err());
            assert_eq!(device.physical_memory_kind(failed), None);
            assert_eq!(device.physical_memory_kind(live), Some(4));
            assert_eq!(device.memory_kinds.len(), 1);
        }
    }

    #[test]
    fn a_failed_free_retains_the_kind_until_the_resource_is_released() {
        let mut device = device();
        device.rm.allocated_memory_kind = 4;
        let (memory, _) = device.alloc_video_memory(0x10000, MemoryKind::PLAIN).unwrap();
        device.rm.refuse_free = Some(memory);
        assert!(device.free(memory).is_err());
        assert_eq!(device.physical_memory_kind(memory), Some(4));
        device.rm.refuse_free = None;
        device.free(memory).unwrap();
        assert_eq!(device.physical_memory_kind(memory), None);
    }

    #[test]
    fn plain_memory_asks_for_no_type_or_attributes() {
        let mut device = device();
        device.alloc_video_memory(0x1000, MemoryKind::PLAIN).unwrap();
        let params = memory_params(&device);
        assert_eq!(u32_in(&params, MEMORY_PARAM_TYPE), 0);
        assert_eq!(u32_in(&params, MEMORY_PARAM_ATTRIBUTES), 0);
        assert_eq!(u32_in(&params, MEMORY_PARAM_FLAGS), MEMORY_FLAG_HANDLE_PROVIDED);
    }

    #[test]
    fn depth_memory_asks_for_the_depth_type_with_its_attributes() {
        let mut device = device();
        device.alloc_video_memory(0x40000, MemoryKind(0x8_0004)).unwrap();
        let params = memory_params(&device);
        assert_eq!(u32_in(&params, MEMORY_PARAM_TYPE), MEMORY_TYPE_DEPTH);
        assert_eq!(u32_in(&params, MEMORY_PARAM_ATTRIBUTES), 0x8_0004);
        assert_eq!(u32_in(&params, MEMORY_PARAM_ATTRIBUTES + 4), 0, "no second attribute word");
        assert_eq!(u32_in(&params, MEMORY_PARAM_OWNER), MEMORY_OWNER);
    }

    #[test]
    fn a_refusal_names_what_failed() {
        let mut device = device();
        device.rm.refuse_class = Some(MEMORY_LOCAL_USER);
        let error = device.alloc_video_memory(0x1000, MemoryKind(0x3A_0002)).unwrap_err();
        assert!(error.to_string().contains("0x40"), "{error}");
    }

    #[test]
    fn only_whole_pages_can_be_exported() {
        let mut device = device();
        assert!(device.export_dmabuf(7, 0x1800).is_err());
        assert!(device.rm.calls.iter().all(|call| !matches!(call, Call::ExportDmabuf { .. })));
        device.export_dmabuf(7, 0x2000).unwrap();
        assert!(device.rm.calls.contains(&Call::ExportDmabuf { memory: 7, size: 0x2000 }));
    }

    #[test]
    fn every_range_is_reserved_in_its_space() {
        let mut device = device();
        let space = device.create_address_space(&[(0x1000, 0x2000), (0x4000, 0x8000)]).unwrap();
        assert_eq!(space.ranges.len(), 2);
        assert_eq!(space.range_at(0x4000), Ok(space.ranges[1].2));
        assert!(space.range_at(0x3000).is_err());
    }

    #[test]
    fn a_mapping_goes_through_the_range_that_holds_it() {
        let mut device = device();
        let space = device.create_address_space(&[(0x1000, 0x2000), (0x4000, 0x8000)]).unwrap();
        let map =
            Mapping { memory: 7, length: 0x1000, address: 0x4000, small_pages: true, ..Mapping::default() };
        device.map(&space, &map).unwrap();
        let mapped = device.rm.calls.iter().find_map(|call| match call {
            Call::MapDma(map) => Some(*map),
            _ => None,
        });
        assert_eq!(mapped.map(|map| map.range), Some(space.ranges[1].2));
    }

    #[test]
    fn typed_alias_changes_the_map_kind_without_reallocating_backing() {
        let mut device = device();
        let space = device.create_address_space(&[(0x1000, 0x10000)]).unwrap();
        let map = Mapping { memory: 7, length: 0x1000, address: 0x4000, small_pages: true, ..Mapping::default() };
        device.map(&space, &map).unwrap();
        let before = device.rm.calls.len();
        let alias = Mapping { address: 0x6000, ..map };
        device.map_with_kind(&space, &alias, Some(4)).unwrap();
        assert!(device.rm.calls[before..].contains(&Call::MapKind { address: 0x6000, kind: 4 }));
        assert!(!device.rm.calls[before..].iter().any(|c| matches!(c, Call::Alloc { .. })));
        assert!(device.rm.calls[before..].iter().any(|c| matches!(c, Call::MapDma(m) if m.memory == 7 && m.address == 0x6000)));
    }

    #[test]
    fn controls_go_to_the_object_their_number_names() {
        let device = device();
        assert_eq!(device.control_target(0x0000_0201), device.client);
        assert_eq!(device.control_target(0x0080_0292), device.device);
        assert_eq!(device.control_target(0x2080_0110), device.subdevice);
    }
    #[test]
    fn video_budget_charges_rm_sizes_and_blocks_before_another_call() {
        let mut d = device();
        d.rm.round_memory_to = 0x10000;
        d.set_video_memory_limit(std::num::NonZeroU64::new(0x20000).unwrap());
        let (a, _) = d.alloc_video_memory(0x1001, MemoryKind::PLAIN).unwrap();
        let (b, _) = d.alloc_video_memory(0x1000, MemoryKind::PLAIN).unwrap();
        assert_eq!(d.video_memory_budget().charged_bytes, 0x20000);
        let calls = d.rm.calls.len();
        assert!(d.alloc_video_memory(1, MemoryKind::PLAIN).is_err());
        assert_eq!(d.rm.calls.len(), calls);
        d.free(a).unwrap();
        assert_eq!(d.video_memory_budget().charged_bytes, 0x10000);
        d.free(b).unwrap();
        assert_eq!(d.video_memory_budget().charged_bytes, 0);
    }

    #[test]
    fn video_budget_rounding_over_limit_rolls_back_and_failed_rollback_stays_charged() {
        for refuse_free in [false, true] {
            let mut d = device();
            d.rm.round_memory_to = 0x10000;
            d.set_video_memory_limit(std::num::NonZeroU64::new(0x8000).unwrap());
            let next = d.next_handle;
            d.rm.refuse_free = refuse_free.then_some(next);
            let error = d.alloc_video_memory(0x1000, MemoryKind::PLAIN).unwrap_err();
            let state = d.video_memory_budget();
            assert_eq!(state.charged_bytes, if refuse_free { 0x10000 } else { 0 });
            assert_eq!(state.failed_rollback_bytes, state.charged_bytes);
            assert!(d.rm.calls.iter().any(|c| matches!(c, Call::Free { handle, .. } if *handle == next)));
            if refuse_free {
                assert!(error.to_string().contains("remains charged"));
                let calls = d.rm.calls.len();
                assert!(d.alloc_video_memory(1, MemoryKind::PLAIN).is_err());
                assert_eq!(calls, d.rm.calls.len());
            }
        }
    }

    #[test]
    fn video_budget_lower_raise_and_failed_free_preserve_existing_objects() {
        let mut d = device();
        let (a, _) = d.alloc_video_memory(100, MemoryKind::PLAIN).unwrap();
        d.set_video_memory_limit(std::num::NonZeroU64::new(50).unwrap());
        assert_eq!(d.video_memory_budget().charged_bytes, 100);
        assert!(d.alloc_video_memory(1, MemoryKind::PLAIN).is_err());
        d.rm.refuse_free = Some(a);
        assert!(d.free(a).is_err());
        assert_eq!(d.video_memory_budget().charged_bytes, 100);
        d.set_video_memory_limit(std::num::NonZeroU64::new(150).unwrap());
        d.alloc_video_memory(50, MemoryKind::PLAIN).unwrap();
        d.rm.refuse_free = None;
        d.free(a).unwrap();
        d.free(a).unwrap(); // Even an RM accepting this twice cannot subtract twice.
        assert_eq!(d.video_memory_budget().charged_bytes, 50);
    }

    #[test]
    fn video_budget_failed_allocations_host_pages_and_other_clients_are_separate() {
        let mut d = device();
        let mut other = device();
        let (a, _) = d.alloc_video_memory(100, MemoryKind::PLAIN).unwrap();
        let (b, _) = other.alloc_video_memory(200, MemoryKind::PLAIN).unwrap();
        assert_eq!(a, b);
        d.alloc_host_pages(0x1000, 0x1000).unwrap();
        for ioctl_failure in [false, true] {
            d.rm.refuse_class = (!ioctl_failure).then_some(MEMORY_LOCAL_USER);
            d.rm.fail_alloc_class = ioctl_failure.then_some(MEMORY_LOCAL_USER);
            assert!(d.alloc_video_memory(100, MemoryKind::PLAIN).is_err());
            assert_eq!(d.video_memory_budget().charged_bytes, 100);
        }
        d.free(a).unwrap();
        assert_eq!(other.video_memory_budget().charged_bytes, 200);
        drop(other);
        assert_eq!(device().video_memory_budget().charged_bytes, 0);
    }

    #[test]
    fn video_budget_exported_objects_remain_charged_without_claiming_fd_lifetime() {
        let mut d = device();
        let (a, _) = d.alloc_video_memory(0x1000, MemoryKind::PLAIN).unwrap();
        let fd = d.export_dmabuf(a, 0x1000).unwrap();
        drop(fd); // A consumer may already have duplicated/sent it; no false observation here.
        d.free(a).unwrap();
        d.free(a).unwrap();
        let state = d.video_memory_budget();
        assert_eq!(state.charged_bytes, 0x1000);
        assert_eq!(state.live_object_bytes, 0);
        assert_eq!(state.retained_export_bytes, 0x1000);
        d.set_video_memory_limit(std::num::NonZeroU64::new(0x1000).unwrap());
        assert!(d.alloc_video_memory(1, MemoryKind::PLAIN).is_err());

        let mut failed = device();
        let (b, _) = failed.alloc_video_memory(0x1000, MemoryKind::PLAIN).unwrap();
        failed.rm.refuse_exports = true;
        assert!(failed.export_dmabuf(b, 0x1000).is_err());
        failed.free(b).unwrap();
        assert_eq!(failed.video_memory_budget().charged_bytes, 0);
    }

}
