# vNVGPU

A source release for an NVIDIA Ampere GPU-sharing integration on Linux/KVM. It includes a buildable Guesthouse daemon workspace and a version-pinned patch series for Proxmox QEMU. The integration exercises Windows GPU paravirtualization (GPU-PV), Linux NVIDIA driver forwarding (`nvfwd`), and a QEMU display path for reims.

## Architecture

| Guest path | Components | Role |
|---|---|---|
| Windows | Virtual Render Device → GPU-PV transport → Guesthouse NVIDIA backend | Graphics, copy and video command handling, adapter statistics and memory accounting |
| Linux | Guest NVIDIA forwarding stack → QEMU `nvfwd` → host NVIDIA driver | Forwarding and per-guest accounting for supported operations |
| macOS | QEMU `reims-graphics-pci` → reims developer library | Display integration. The library is a separate build dependency |

## Build the Guesthouse daemon

On x86-64 Linux with Rust 1.88 or newer and a native linker:

```sh
cd guesthouse
cargo test --locked --lib -p guesthouse-backend -p guesthouse-core -p guesthouse-backend-nvidia
cargo build --locked --release -p guesthouse-daemon --features nvidia
```

The packaged tree passed 771 library tests (16 backend, 403 core, 352 NVIDIA backend) and the release build. Real GPU tests require a supported host driver and guest setup. See [build and test notes](docs/build-and-test.md) for the QEMU base version and historical hardware results.

## Scope and results

- Windows Ampere bring-up covers graphics, copy, video channels, virtual display modes and opt-in GPU-PV protocol 43 handling.
- The memory tests cover private USERD cleanup, retained exports, owner-scoped accounting and local/nonlocal allocation classification.
- GPU statistics handlers return size-correct replies. The source includes the tested budget and local segment allocator paths. Task Manager GPU graphs remain an open problem for the tested VirtualRender configuration.
- Prior hardware tests compared 600 distinct decoded frames per Windows CUVID and D3D11VA path with a software reference. These results apply to the earlier hardware-tested candidate. The published snapshot was verified separately with library tests and a release build.
- The QEMU series was reversed and reapplied with zero fuzz. All 76 affected paths matched the recorded build tree. QEMU packaging and guest deployment depend on an independently prepared Proxmox build environment.

## Provenance and licenses

The Rust workspace is derived from [Guesthouse](https://github.com/xnixy/guesthouse). The preserved local baseline records commit `86bf70077aab3ebb696d43ee9433c4f0b95e8465` and tree `fc3d2e64570c68f6b47917b515c2330189f6282e`. The workspace is vendored so it can be rebuilt without access to that commit. Licensing and third-party notices accompany the [Guesthouse source](guesthouse/LICENSE) and [third-party notices](guesthouse/THIRD-PARTY.md). The QEMU patches build on [QEMU](https://www.qemu.org/) and [Proxmox pve-qemu](https://git.proxmox.com/?p=pve-qemu.git;a=summary). See the [QEMU license](patches/pve-qemu/LICENSE), [GPL text](patches/pve-qemu/COPYING) and [packaging copyright](patches/pve-qemu/debian/copyright). Individual notices remain applicable.
