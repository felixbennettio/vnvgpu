# Build and test notes

The daemon source builds independently of the QEMU device. Start with the commands in the root README. For a QEMU integration build, prepare the source package for `pve-qemu-kvm 11.1.3-2` from the Proxmox package archive, with its stock Quilt series applied. The integration series assumes that base. The reims device needs a separately installed compatible development library. Apply the series in order from the prepared QEMU source tree:

```sh
PATCH_DIR=/absolute/path/to/vnvgpu/patches/pve-qemu
while IFS= read -r patch_name; do
    patch --batch --forward --fuzz=0 -p1 -i "$PATCH_DIR/$patch_name" || exit 1
done < "$PATCH_DIR/series"
```

Build according to the Proxmox package instructions for that source version. On a build with the relevant integrations enabled, run the device discovery checks and the QEMU display qtest:

```sh
for device in reims-graphics-pci nvfwd gpupv-render; do
    ./qemu-system-x86_64 -device "$device,help" || exit 1
done
QTEST_QEMU_BINARY="$PWD/qemu-system-x86_64" ./tests/qtest/display-screendump-test
```

## Test cases for an isolated GPU host

| Test | Pass criterion | Recorded result |
|---|---|---|
| Private USERD lifetime | Repeatedly create and close an owned graphics channel. The final charge matches baseline | Prior hardware control retained 1 MiB after 16 cycles. With cleanup, the charge returned to zero |
| External export lifetime | Retain an export while freeing the original allocation and test admission at the limit | The held export remained charged. Release of private objects made capacity reusable |
| Process accounting | Compare an allocating D3D process with an independent observing process | The observer remained at zero through 8 and 32 MiB allocation trials. The 5 GiB limit remained in effect |
| H.264 hardware output | Decode the same distinct frames through a software reference and each Windows hardware path | All 600 NV12 frames matched for CUVID. All 600 also matched for D3D11VA |
| Statistics reply | Compare guest results and the corresponding host counter. Check rejection separately | Wire size, status, process ownership and local memory accounting were verified. The native GPU Performance page remained unresolved |

Use two observations for any hardware acceptance result. An application status establishes its API behavior. A corresponding fence, byte comparison, hardware counter or memory ledger establishes what happened on the host. Record mixed-load or long-run acceptance separately from short functional checks.
