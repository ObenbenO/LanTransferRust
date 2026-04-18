# LanTransfer（lan_transfer）

同一局域网内快速传文件；在对方允许时可发起远程协助（查看与控制）。桌面端使用 **Rust + egui（eframe）** 实现，Windows 下可打包为单 exe 绿色分发（数据与日志写在 exe 旁目录）。

**English:** LAN file transfer and optional remote assistance, built with Rust and egui (eframe). Release builds on Windows use the GUI subsystem (no extra console window).

---

## 功能概览

- **设备发现**：UDP 广播（默认端口 `45678`），同网段内自动发现其他已启动本应用的设备。
- **文件传输**：多文件、单连接 TCP 协议；发送进度、接收记录；支持按用户或按分组标签批量发送。
- **远程协助**：被控端在设置中开启后，协助端从列表进入全屏远程画面（截屏 `xcap`，键鼠 `enigo`）。
- **本机设置**：昵称与分组标签、接收目录、远程协助开关等持久化到本地数据目录。

## 环境要求

- **Rust**：稳定版工具链（`edition = "2021"`），并已安装 **Windows MSVC** 目标（`rustup default stable` 后一般即可）。
- **操作系统**：当前开发与发布以 **Windows 10/11（x86_64-pc-windows-msvc）** 为主；依赖栈中含跨平台库，其他平台需自行验证构建与权限（防火墙、屏幕录制等）。

## 如何运行

### 从源码运行（开发调试）

在仓库根目录执行：

```bash
cargo run --bin lan_transfer
```

等同于先 `cargo build` 再运行 debug 产物。Windows 下 **debug 会附带控制台窗口**，便于查看 `stdout`/`stderr`。

若只想编译 debug 而不自动启动：

```bash
cargo build --bin lan_transfer
```

可执行文件路径：`target/debug/lan_transfer.exe`（Windows）。

### 运行已发布的 exe（最终用户）

1. 将 **`lan_transfer.exe`** 放到任意文件夹（例如桌面或自建的 `LanTransfer` 目录）。
2. 双击运行。首次启动会在 **exe 同目录** 下自动创建 **`lan_transfer_data`**（配置、设备 ID、默认接收目录等），无需事先手动建目录。
3. 若系统或安全软件弹出防火墙提示，请选择 **允许** 访问专用网络，否则局域网发现或文件传输可能失败。

**说明**：使用 **`cargo build --release`** 打出的 exe 在 Windows 上 **不会** 再弹出黑色日志窗口；日常日志在 `lan_transfer_data/logs/`（见下文）。

## 如何打包与分发

### 1. 编译 Release

在仓库根目录执行：

```bash
cargo build --release --bin lan_transfer
```

### 2. 取产物

- 主程序：**`target/release/lan_transfer.exe`**

该文件可 **单独** 作为「绿色包」分发：用户只需这一个 exe，数据与日志均在运行目录自动生成。

### 3. 建议的分发方式（任选）

| 方式 | 做法 |
|------|------|
| **仅 exe** | 将 `lan_transfer.exe` 发给对方或放到网盘；最简单。 |
| **exe + 说明** | 同上，另附简短说明（防火墙、需同一局域网等）；可把本 README 节选成 `使用说明.txt`。 |
| **打 zip** | 将 `lan_transfer.exe`（及可选说明）打包为 zip，便于下载。PowerShell 示例：`Compress-Archive -Path .\target\release\lan_transfer.exe -DestinationPath .\LanTransfer_windows_x64.zip` |

**不必** 把 `lan_transfer_data` 打进 zip：那是用户机器上的本地数据；若误打包了旧数据，反可能造成配置冲突。

### 4. 与 GitHub / CI 的关系

从源码克隆的用户应本地执行上述 `cargo build --release`；若日后配置 GitHub Actions，可在 workflow 里调用相同命令，并把 `target/release/lan_transfer.exe` 作为 Artifact 上传。

## 诊断日志

运行中的诊断类日志写入：

`lan_transfer_data/logs/`

（例如文件传输、远程桌面相关 `.log`）。发布版用户无需控制台即可在本机查阅。

## 仓库结构（简要）

| 路径 | 说明 |
|------|------|
| `src/main.rs` | 可执行入口、egui 界面与 `eframe::App` 实现 |
| `src/lib.rs` | 库根，挂载内部模块 |
| `src/api/` | 对外能力封装（配置、对端列表、传输与远程 API 等） |
| `src/app_state.rs` | 全局状态、数据目录与诊断日志路径 |
| `src/file_transfer.rs` | 局域网 TCP 文件协议 |
| `src/remote_desktop.rs` | 远程协助协议与截屏/键鼠 |

## 技术栈

- **UI**：`eframe` 0.30（`egui` + **glow** 后端、`default_fonts`）
- **异步**：`tokio`（`rt-multi-thread`）
- **序列化**：`serde` / `serde_json`
- **其它**：`rfd`（系统文件对话框）、`image`（JPEG 等）、`xcap`、`enigo`、`get_if_addrs` 等

## 许可

仓库若未包含 `LICENSE` 文件，则版权与许可条款尚未声明；上传到 GitHub 前建议自行补充许可证（例如 MIT、Apache-2.0 等）。

---

如有问题或改进建议，欢迎通过 GitHub Issues 讨论。
