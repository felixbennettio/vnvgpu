# vNVGPU

vNVGPU 是一个面向 NVIDIA Ampere 的 Linux/KVM 多来宾 GPU 共享集成项目。Windows 通过 Guesthouse GPU-PV 提交图形与视频工作，Linux 通过 QEMU `nvfwd` 转发 NVIDIA 驱动操作，macOS 使用独立 reims 库提供的 QEMU 显示设备。[English README](README.md)

## 主要工作

- **Windows GPU-PV。** 对照公开 SDK 和客户端协议，核对消息长度、状态码和功能作用范围。修复统计拒绝应答长度后，Windows 能读到预期的“不支持”状态，再逐步加入预算和分配统计。
- **Ampere 渲染与视频。** 调整图形、复制和视频通道的父对象、USERD 映射及调度路径。早期实机候选版的 CUVID 和 D3D11VA 各有 600 帧 NV12 输出与软件参考逐帧一致。
- **显存回收与归属。** 修复连续关闭私有通道后仍残留的预算扣款，保留仍由外部引用的导出对象的计费。通过独立持有者和观察者验证进程归属，8 MiB 和 32 MiB 对照中观察者读数保持为零。
- **QEMU 集成。** 将 47 个补丁固定到 Proxmox QEMU `11.1.3-2` 基线。以零模糊度撤回并重放后，76 个受影响路径的内容与原构建树一致。

## 构建和复现

在 x86-64 Linux 上使用 Rust 1.88 或更高版本。

```bash
cd guesthouse
cargo test --locked --lib -p guesthouse-backend -p guesthouse-core -p guesthouse-backend-nvidia
cargo build --locked --release -p guesthouse-daemon --features nvidia
```

公开的 Rust 源码快照通过了 771 项库测试和 release 构建。实机复现还需准备匹配的驱动、QEMU 和来宾组件。原生 Windows 任务管理器的 RTX 曲线，以及三来宾 4:1:1 调度仍待验证。

| 阅读入口 | 内容 |
|---|---|
| [架构](docs/architecture.md) | 各系统的设备、协议与后端关系 |
| [构建说明](docs/build-and-test.md) | QEMU 补丁顺序、版本与测试命令 |
| [协议调试](docs/protocol-debugging.md) | 怎样从接口定义与实测结果定位协议问题 |
| [显存案例](docs/memory-accounting.md) | 预算回收、共享引用和进程归属 |
| [验证矩阵](docs/validation.md) | 已完成的测试与验收范围 |
| [探针源码](probes/README.md) | Windows API 探针和 NVIDIA 通道回收测试 |

Rust 代码及许可说明见 [Guesthouse LICENSE](guesthouse/LICENSE) 和 [第三方声明](guesthouse/THIRD-PARTY.md)。QEMU 补丁的许可参见 [QEMU LICENSE](patches/pve-qemu/LICENSE) 和各源文件声明。
