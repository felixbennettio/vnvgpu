//! Diagnostics: take frames from a VM's frames.sock and write the last one as a PPM.
//! usage: grab RUN_FOLDER OUT.ppm SECONDS
use guesthouse_frame_socket::{Format, FrameSocket, Hello, Message, Release};
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::time::{Duration, Instant};

const XR24: u32 = u32::from_le_bytes(*b"XR24");
const AR24: u32 = u32::from_le_bytes(*b"AR24");

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let socket = FrameSocket::connect(&Path::new(&args[1]).join("frames.sock")).expect("connect");
    let formats = vec![Format { fourcc: XR24, modifier: 0 }, Format { fourcc: AR24, modifier: 0 }];
    socket
        .send(&Message::Hello(Hello { formats, refresh_millihertz: 60000, peak_nits: 0, wants_memory: true }), &[])
        .expect("hello");
    socket.set_nonblocking(true).unwrap();
    let mut slots: Vec<OwnedFd> = Vec::new();
    let mut size = 0u64;
    let (mut frames, mut last) = (0u64, None);
    let end = Instant::now() + Duration::from_secs(args[3].parse().unwrap());
    while Instant::now() < end {
        match socket.receive() {
            Ok(Some(packet)) => match packet.message {
                Message::Ring(ring) => {
                    println!("ring generation {} slots {} size {}", ring.generation, ring.slots, ring.size);
                    slots = packet.fds;
                    size = ring.size;
                }
                Message::Frame(frame) => {
                    frames += 1;
                    let d = frame.descriptor;
                    if frames == 1 {
                        println!("first frame {}x{} stride {} fourcc {:#x}", d.width, d.height, d.stride, d.format.fourcc);
                    }
                    if let Some(fd) = slots.get(frame.slot as usize) {
                        let ptr = unsafe {
                            libc_mmap(fd.as_raw_fd(), size as usize)
                        };
                        if !ptr.is_null() {
                            let bytes = unsafe { std::slice::from_raw_parts(ptr.add(d.offset as usize), (d.stride * d.height) as usize) };
                            last = Some((d.width, d.height, d.stride, bytes.to_vec()));
                            unsafe { libc_munmap(ptr, size as usize) };
                        }
                    }
                    let _ = socket.send(&Message::Release(Release { generation: frame.generation, slot: frame.slot }), &[]);
                }
                _ => {}
            },
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => { println!("receive: {e}"); break; }
        }
    }
    println!("frames {frames} in {} s", args[3]);
    if let Some((w, h, stride, bytes)) = last {
        let mut out = File::create(&args[2]).unwrap();
        write!(out, "P6\n{w} {h}\n255\n").unwrap();
        for y in 0..h as usize {
            for x in 0..w as usize {
                let p = &bytes[y * stride as usize + x * 4..][..4];
                out.write_all(&[p[2], p[1], p[0]]).unwrap();
            }
        }
    }
}

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}
unsafe fn libc_mmap(fd: i32, len: usize) -> *mut u8 {
    let p = unsafe { mmap(std::ptr::null_mut(), len, 1, 1, fd, 0) };
    if p as isize == -1 { std::ptr::null_mut() } else { p }
}
unsafe fn libc_munmap(p: *mut u8, len: usize) {
    unsafe { munmap(p, len) };
}
