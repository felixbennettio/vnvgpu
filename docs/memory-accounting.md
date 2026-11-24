# Memory accounting and lifetime

| Quantity | Meaning |
|---|---|
| Admission limit | Maximum bytes admitted by the VM's configured local budget |
| Charged bytes | Live charge including resources retained for an external consumer |
| Process usage | Allocations attributed to the queried guest owner |
| Residency | Bytes physically resident at the chosen observation point. This requires a corresponding measurement |

## Private USERD release

A graphics channel uses a USERD allocation for user-accessible queue control data. The backend maps private memory for this purpose. In the original implementation an additional export reference could outlive the CPU mapping. That left a residual charge after repeated channel closure. The update closes the private export descriptor once its mapping is established and unmaps private memory before returning its charge. An independently held export keeps its charge until the relevant lifetime ends.

The positive test closes 16 channels. Its original residual was 1 MiB and the updated run returned to baseline. The negative control retains an external export, fills the remaining budget and checks that admission fails. A subsequent allocation exercises capacity recovery.

## Process ownership

An earlier query summed allocation sizes across the session. The corrected path filters by the requesting process and reports local and system-backed classes separately. The regression includes the creator and importer releasing a shared allocation in both orders.

A hardware probe creates a D3D11 buffer in one process and samples a second process that only queries DXGI. With 8 MiB and 32 MiB allocations, the observer remained at zero and the 5 GiB limit stayed intact. The [probe guide](../probes/README.md) includes the command sequence.
