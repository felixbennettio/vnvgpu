# Validation matrix

| Area | Evidence | Status |
|---|---|---|
| Source release | 16 backend, 403 core and 352 NVIDIA backend library tests, plus release compilation | Verified on the published Rust source snapshot |
| QEMU patch replay | Reverse and forward application of 47 patches with zero fuzz. All 76 touched paths matched the recorded build tree | Verified against the prepared Proxmox source |
| Video decode | 600 Windows CUVID frames and 600 Windows D3D11VA frames matched their respective software references | Hardware-tested on an earlier candidate |
| Display and remote access | Device enumeration, display qtests and a Windows remote desktop startup check | Historical integration tests |
| Video-memory admission | Lifetime and accounting checks with private and exported allocations | Hardware-tested and covered by Rust regressions |
| Direct Windows statistics | Budget, segment and aggregate graphics time queries compared with host observations | Selected queries verified. Some instrumentation remains experimental |
| Windows Task Manager | GPU Performance panel and PerfLib instance checked using the same adapter identity | RTX Performance entry remains open |
| Weighted sharing under contention | Concurrent guest workloads with normalized throughput | Target 4:1:1, priority interleaving and idle borrowing remain open |

## Reproduction order

1. Build the daemon and run the library tests on Linux.
2. Replay the QEMU series on the matching Proxmox base. Confirm device discovery with the integration enabled.
3. Run the [probes](../probes/README.md) one at a time with host observations collected in the same window.
4. Separate functional tests, resource-recovery tests and mixed-load testing in the results. For decoding, compare distinct input frames to a software reference after both outputs use the same pixel format.

Build and deployment details are in the [build and test guide](build-and-test.md).
