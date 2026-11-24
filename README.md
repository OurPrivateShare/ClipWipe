# ClipWipe

ClipWipe 是 Windows 和 Linux 的剪贴板清理工具。复制内容后默认保留 30 秒，到期后清空当前剪贴板。也可以将内容替换成 32 个随机十六进制字符。

## 下载安装包

发布包位于 `dist/`，解压后进入包含可执行文件的目录。

| 系统 | 发布包 |
| --- | --- |
| Windows x64 | `ClipWipe-0.1.0-windows-x64.zip` |
| Linux x64（glibc 2.34 或以上） | `ClipWipe-0.1.0-linux-x64.tar.gz` |

`dist/` 还提供 `windows-x64/clipwipe.exe` 和 `linux-x64/clipwipe`。Windows ZIP 解压后的目录名为 `ClipWipe-0.1.0-win-x64`。两个压缩包均附有使用说明和更新记录，Linux 压缩包已保留可执行权限。若单独复制 Linux 二进制，可用 `chmod +x clipwipe` 补上执行权限。

`SHA256SUMS.txt` 记录压缩包及可执行文件的 SHA-256。在压缩包所在目录执行对应命令，再与清单核对。

```powershell
Get-FileHash -Algorithm SHA256 .\ClipWipe-0.1.0-windows-x64.zip
```

```sh
sha256sum ClipWipe-0.1.0-linux-x64.tar.gz
```

## 常用命令组合

### 安装并设置开机启动

系统安装需要管理员权限。安装一次后，各用户可在自己的终端使用 `clipwipe`。

Windows 在管理员 PowerShell 中进入解压目录后运行。

```powershell
.\clipwipe.exe install
```

Windows 会安装到 `%ProgramFiles%\ClipWipe` 并更新系统 `PATH`。装完后从开始菜单重新打开日常使用的终端。

Linux 在解压目录运行。

```sh
sudo ./clipwipe install
```

Linux 安装路径为 `/usr/local/bin/clipwipe`。

安装后，在需要自启的账号下运行下面两条命令。ClipWipe 从该用户下次登录图形桌面起持续监听，默认在每次复制后等待 30 秒再清空。

```sh
clipwipe --version
clipwipe autostart enable
```

### 调整开机启动的清理方式

再次启用自启会更新下次登录时使用的参数。例如每次复制后等待 60 秒，再用随机文本覆盖。

```sh
clipwipe autostart enable --timeout 60 --mode random
```

### 本次手动运行

```sh
clipwipe clear
clipwipe watch --timeout 15 --mode clear
```

第一行立即清空一次，第二行在当前终端以 15 秒倒计时持续运行。按 `Ctrl+C` 结束前台监听。

### 关闭开机启动或卸载

取消下次登录时的启动。

```sh
clipwipe autostart disable
```

全机卸载前，各用户可用上面的命令关闭自己的自启。更新或卸载前先结束当前监听。Windows 的后台监听可在任务管理器结束 `clipwipe.exe`。

Windows 在管理员终端运行。

```sh
clipwipe uninstall
```

Linux 运行。

```sh
sudo /usr/local/bin/clipwipe uninstall
```

## 命令速查

运行 `clipwipe` 或 `clipwipe --help` 查看版本和可复制的命令示例。未安装时，用 Windows PowerShell 的 `.\clipwipe.exe` 或 Linux 的 `./clipwipe` 代替。

| 命令 | 用途 |
| --- | --- |
| `clipwipe clear` | 立即清空当前剪贴板 |
| `clipwipe clear --mode random` | 立即写入随机文本 |
| `clipwipe watch` | 在前台监听，每次复制后重新计时，按 `Ctrl+C` 退出 |
| `clipwipe watch --timeout 60 --mode random` | 每次复制 60 秒后写入随机文本 |
| `clipwipe install` | 安装到系统，供所有用户使用 |
| `clipwipe autostart enable --timeout 30 --mode clear` | 下次登录图形桌面后自动监听 |
| `clipwipe autostart disable` | 关闭后续登录自启 |
| `clipwipe uninstall` | 卸载系统安装 |

`--timeout` 使用正整数秒。`watch` 启动时如果已有内容，会从启动时算起。手动运行的参数仅对本次生效。再次启用自启可更新下次登录时使用的参数。

## 桌面环境

- Windows 使用当前交互式用户会话的剪贴板。
- Linux 支持带 XFixes 的 X11，以及提供 `ext-data-control` 或 `wlr-data-control` 的原生 Wayland 会话。缺少该接口的 Wayland 桌面会提示无法访问剪贴板。登录自启由桌面的 XDG 自启功能启动。
- 倒计时适用于文本、图片和文件列表。ClipWipe 跟踪变化通知，不读取原内容。清理范围是当前复制粘贴使用的剪贴板，历史记录、云同步副本与 Linux 鼠标选区由各自的应用或系统功能管理。
- Linux 的即时随机覆盖会由后台进程提供粘贴内容，下一次复制会让该进程退出。Wayland 在系统通知和写入之间可能发生并发复制。

## 构建

安装 Rust 和目标系统的链接工具链，运行 `cargo build --release --locked`。本机构建的可执行文件位于 `target/release/`。`cargo test --locked` 运行测试。

版本记录见 [UPDATE.md](UPDATE.md)。
