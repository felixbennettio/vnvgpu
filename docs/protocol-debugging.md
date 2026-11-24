# Protocol debugging

## Case study: statistics refusal on GPU-PV

A statistics request initially produced a four-byte error response. The Windows API surfaced a generic failure. The working hypothesis was that the caller expected a fixed-size result record before interpreting the status.

The investigation compared public Windows SDK types, open client-side GPU-PV structures, and observations from a minimal Windows probe. Each query used the same guest build and adapter identity. Feature checks distinguished global capability queries from per-adapter capability queries. Tests observed both the guest's result and the host handler before changing the advertised features.

The corrected refusal response is `0x310` bytes and carries `STATUS_NOT_SUPPORTED`. A regression test checks the length, status and cleared payload. The guest subsequently exposed the intended status. This allowed memory-query support to be added in a separate increment with its own owner and segment tests.

## What to measure on hardware

- Retain the guest build, adapter identity and queried capability with the result.
- Verify HRESULT or NTSTATUS before trusting a response payload.
- Use controlled allocation and release for memory metrics, and completed work plus a reference output for execution tests.
- Treat DXCore enumeration, direct KMT queries, PerfLib instances and the Task Manager Performance view as distinct checks.

RTX enumeration and some direct KMT metrics succeeded in the research environment. Task Manager still showed no RTX Performance entry. A cold-boot experiment changed the feature response successfully while the observed PerfLib adapter list stayed the same. That isolates the remaining issue to another part of the registration path.
