# PSK 代理桌面客户端（Windows 单文件便携）

这是唯一的图形客户端：Tauri 2 壳（Rust + 系统 WebView）。真正跑代理的仍是仓库根
`src/bin/psk-client.rs`（以 sidecar 子进程方式启动）。传输层、PSK 认证、QUIC
一行未改。

```
apps/desktop/
├── launcher/                   # 单文件引导器（Rust）：解压载荷到本地缓存并启动 PSKProxy.exe
├── src-ui/                     # 静态前端（无打包器，Tauri 直接吃这个目录）
├── src-tauri/                  # Rust 壳
│   ├── src/main.rs             # 启动/停止 sidecar、转发日志事件
│   ├── tauri.conf.json         # externalBin、图标、窗口、CSP
│   └── binaries/               # sidecar 输入（带目标三元组后缀，构建前放入）
├── scripts/
│   ├── build_windows.ps1       # sidecar 准备 → tauri build → 单文件 exe
│   ├── prepare_sidecar.ps1     # 单独把 psk-client.exe 放进 binaries/
│   └── bundle_windows_portable.ps1  # 引导器 + zip 载荷 + 尾部 → 单文件 exe
└── icons/                      # 由 scripts 下方的 PIL 脚本生成
```

## 界面

左栏三个页面：

- **连接**：服务端地址/端口、预共享密钥（可显示）、本地 SOCKS5 地址/端口、QUIC
  连接数（高 RTT 下并发下载更快）、空闲超时。配置存在 WebView 的 localStorage。
- **状态**：运行状态、SOCKS5 入口、服务端、QUIC 连接数。
- **日志**：sidecar 的 stdout/stderr 逐行显示，带行数统计与清空。

启动 sidecar 时只传 `--server`、`--socks` 与 `--psk=<密钥>`，其余用默认值。关窗或异常退出时用
Windows Job Object 把子进程一并终止，不会残留占用 SOCKS5 端口。

## Windows 构建（便携单文件）

```powershell
# 1) 先构建代理客户端（仓库根）
cargo build --release

# 2) 一键打包（在已装好 Rust/Node 的环境）
powershell -File apps/desktop/scripts/build_windows.ps1 `
  -ClientBin target\release\psk-client.exe
```

产物：`dist/PSKProxy-<版本>-win64.exe`。首次双击时引导器把 `PSKProxy.exe` +
`psk-client.exe` 解到 `%LOCALAPPDATA%\PSKProxy\bin\<载荷哈希>\`，之后直接启动
缓存；换新版本按内容哈希分目录，旧目录 7 天未用后清理。不写系统目录、不建卸载
项、不需要管理员权限，删除 exe 即卸载。

配置在 WebView 的 localStorage（按 `com.psk.proxy` 标识），升级不丢配置；不要改
`src-tauri/tauri.conf.json` 的 `identifier`。

排障：`PSKProxy.exe --psk-launcher-selftest`（写
`%LOCALAPPDATA%\PSKProxy\psk-launcher-selftest.txt`）；失败日志在 launcher.log。

依赖：Rust（`cargo` 在 PATH）、Node/npm。Windows 10/11 自带 WebView2 运行时。

## 开发

```bash
# 用已构建的 psk-client，避免重新打包 sidecar：
cd apps/desktop
npm install
PSKPROXY_CLIENT_BIN=D:\psk-quic-proxy\target\release\psk-client.exe npx tauri dev
```

`PSKPROXY_CLIENT_BIN` 覆盖 sidecar 路径，便于开发期直接跑 dev 模式。