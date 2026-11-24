use guesthouse_backend_nvidia::{
    channel::{Channel, ChannelPlace, Engine, ListChannel, LIST_AREA},
    profile::{AMPERE, HOST_615},
    rm::{device::Device, linux::LinuxRm},
};
use std::num::NonZeroU64;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let version = std::env::args().nth(1).ok_or("driver version required")?;
    if std::env::var_os("GH_TEST_MAPPED_GRAPHICS_USERD").is_none() {
        return Err("enable mapped graphics USERD for this diagnostic".into());
    }
    let rm = LinuxRm::open_card(&version, &HOST_615, 0)?;
    let mut device = Device::open(rm, &HOST_615)?;
    device.set_video_memory_limit(NonZeroU64::new(8 << 20).unwrap());
    let base = 0x1_4200_0000;
    let space = device.create_address_space(&[(base, base + 0x200_0000)])?;
    for index in 1..=16 {
        let place = ChannelPlace {
            ring_address: ListChannel::ring_address(base),
            ring_entries: 4096,
            area_address: base,
            area_size: LIST_AREA,
        };
        let channel = Channel::new(&mut device, &AMPERE, &HOST_615, &space, place, Engine::Graphics)?;
        channel.close(&mut device, &space)?;
        if index == 1 || index == 16 {
            let state = device.video_memory_budget();
            println!("channels_closed={index} live={} retained={} charged={}",
                     state.live_object_bytes, state.retained_export_bytes, state.charged_bytes);
        }
    }
    device.destroy_address_space(&space)?;
    Ok(())
}
