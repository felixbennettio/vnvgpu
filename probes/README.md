# Hardware validation probes

These programs supplement the Rust tests. Use a compatible guest with the NVIDIA render adapter visible. Compiler and probe output depend on the local driver stack.

## Windows

Compile using an MSYS2 UCRT64 shell.

```bash
cd probes/windows
g++ -std=c++17 -O2 windows-memory-owner-check.cpp -ld3d11 -ldxgi -ldxguid -o owner.exe
OWNER_PROBE_MIB=8 bash run-windows-memory-owner-check.sh ./owner.exe
OWNER_PROBE_MIB=32 bash run-windows-memory-owner-check.sh ./owner.exe
g++ -std=c++17 -O2 windows-d3d12-residency-check.cpp -ld3d12 -ldxgi -ldxguid -o residency.exe
./residency.exe
```

The owner test prints phase, usage and budget. Compare `before`, `held` and `released` with the independent `observer` rows. The residency program exercises D3D12 residency calls. Pair its API result with a separate host memory measurement when validating residency.

The PowerShell probes inspect DXCore enumeration and the statistics capability through the public Windows interfaces. Run them from a Windows PowerShell terminal.

```powershell
.\windows-dxcore-enumeration.ps1
.\windows-kmt-statistics-feature.ps1
```

## NVIDIA backend

Run the example programs on the Linux host with an appropriate NVIDIA driver. Copy the sources into the workspace examples directory for the duration of the test.

```bash
cd guesthouse
cp ../probes/nvidia/private-userd-budget-probe.rs crates/backend-nvidia/examples/private_userd_budget.rs
cp ../probes/nvidia/verify-private-userd-budget.rs crates/backend-nvidia/examples/verify_private_userd_budget.rs
cargo build --locked --release -p guesthouse-backend-nvidia --examples
GH_TEST_MAPPED_GRAPHICS_USERD=1 target/release/examples/private_userd_budget 615.78.08
GH_TEST_MAPPED_GRAPHICS_USERD=1 target/release/examples/verify_private_userd_budget 615.78.08
```

The version argument must match the host driver. The first program compares channel lifetime and the second checks budget recovery with exported and private objects. The historical observations are in the [memory accounting case study](../docs/memory-accounting.md).

The probe sources use the GPL-2.0-or-later terms described in the [workspace license](../guesthouse/LICENSE).
