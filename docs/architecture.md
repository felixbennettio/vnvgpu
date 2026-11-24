# Architecture

The integration targets NVIDIA Ampere on an x86-64 KVM host. Each guest sees a device appropriate to its driver stack. QEMU supplies the guest-facing device and memory transport. The host NVIDIA driver operates the GPU.

| Layer | Windows GPU-PV | Linux forwarding | macOS display |
|---|---|---|---|
| Guest interface | VirtualRender and vendor user-mode driver | NVIDIA Linux driver and forwarding stack | Guest reims display driver |
| QEMU component | Hyper-V graphics channels and shared apertures | `nvfwd` device and object tracking | `reims-graphics-pci` |
| Host backend | Guesthouse NVIDIA backend | Supported operations forwarded to the host driver | Compatible reims library |
| Representative checks | Resource ownership, fences, decoded frames | Device enumeration, memory admission, console scanout | Display device integration |

## Windows request flow

1. The guest submits a request through the graphics channel.
2. QEMU transfers it to the Guesthouse daemon. The session layer parses requests and tracks guest allocation and process identifiers.
3. The NVIDIA backend creates or updates host resources. Queues share mapped memory with the backend where required.
4. The guest receives a protocol response or observes a fence. These signals answer different questions. The response checks request handling and the fence checks completion of queued work.

The display path and GPU execution have different checkpoints. Basic device enumeration can work while a video queue is unable to run. A rendered frame can reach a guest capture path while the host console scanout still has a separate integration issue.

## Ownership and recovery

The QEMU patch stack includes channel teardown and backend reconnection handling. The payload and mappings used by a request have a bounded lifetime. Memory admission tracks actual charges and retained exports. Per-process queries use guest ownership metadata, while backend budget counters describe the VM allocation pool. See [memory accounting](memory-accounting.md).
