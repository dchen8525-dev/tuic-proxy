# TUIC Proxy 桌面客户端（Windows / Linux 便携版）

这是图形客户端：Tauri 2 壳（Rust + 系统 WebView）。真正跑代理的仍是仓库根
`src/bin/tuic-proxy-client.rs`（以 sidecar 子进程方式启动）。传输层、PSK 认证、
QUIC 一行未改。

```
apps/desktop/
├── launcher/                   # Windows 单文件引导器（Rust）：解压载荷到本地缓存并启动 GUI
├── src-ui/                     # 静态前端（无打包器，Tauri 直接使用这个目录）
├── src-tauri/                  # Rust 壳
│   ├── src/main.rs             # 启动/停止 sidecar、转发日志事件
│   ├── tauri.conf.json         # externalBin、图标、窗口、CSP
│   └── binaries/               # sidecar 输入（带目标三元组后缀，构建前放入）
├── scripts/
│   ├── build_windows.ps1       # sidecar 准备 → tauri build → 单文件 exe
│   ├── prepare_sidecar.ps1     # 把 tuic-proxy-client.exe 放进 binaries/
│   ├── prepare_sidecar.sh      # 同上，Linux
│   └── bundle_windows_portable.ps1  # 引导器 + zip 载荷 + 尾部 → 单文件 exe
└── icons/                      # 应用图标
```

## 界面

左栏三个页面：

- **连接**：服务端地址/端口、预共享密钥、本地 SOCKS5 地址/端口。
- **状态**：运行状态、SOCKS5 入口、服务端。
- **日志**：sidecar 的 stdout/stderr 逐行显示，带行数统计与清空。

启动 sidecar 时只传 `--server`、`--socks` 与 `--psk=<密钥>`，其余用默认值。
配置存在 WebView 的 localStorage（`com.tuic.proxy`）。关窗或异常退出时一并结束
sidecar（Windows 用 Job Object，Linux 用 SIGTERM），不会残留占用 SOCKS5 端口。

## Windows 构建（单文件便携）

```powershell
# 1) 先构建代理客户端（仓库根）
cargo build --release

# 2) 一键打包（需要 Rust 与 Node/npm）
powershell -File apps/desktop/scripts/build_windows.ps1 `
  -ClientBin target\release\tuic-proxy-client.exe
```

产物：`dist/tuic-proxy-gui-<版本>-windows-x64.exe`。首次双击时引导器把
`tuic-proxy-gui.exe` + `tuic-proxy-client.exe` 解到
`%LOCALAPPDATA%\TUICProxy\bin\<载荷哈希>\`，之后直接启动缓存；换新版本按内容
哈希分目录，旧目录 7 天未用后清理。不写系统目录、不建卸载项、不需要管理员权限。

排障：`tuic-proxy-gui-<版本>-windows-x64.exe --tuic-proxy-launcher-selftest`
（写 `%LOCALAPPDATA%\TUICProxy\tuic-proxy-launcher-selftest.txt`）；失败日志在
`%LOCALAPPDATA%\TUICProxy\launcher.log`。

## Linux 构建（AppImage）

```bash
# 依赖（Ubuntu 22.04/24.04）
sudo apt-get install -y libwebkit2gtk-4.1-dev libayatana-appindicator3-dev \
  librsvg2-dev patchelf libfuse2

# 1) 构建代理客户端并放到 Tauri 的 sidecar 目录
cargo build --release
sh apps/desktop/scripts/prepare_sidecar.sh target/release

# 2) 构建 AppImage
cd apps/desktop
npm ci
npx tauri build --bundles appimage
```

产物：`apps/desktop/src-tauri/target/release/bundle/appimage/
tuic-proxy-gui_<版本>_amd64.AppImage`（Tauri 的默认命名）。CI 发布时会复制成
`tuic-proxy-gui-<版本>-linux-amd64.AppImage`，与 deb / Windows exe 的命名保持一致。
`chmod +x` 后双击即可运行，不安装；系统没有 FUSE 2 时可用
`APPIMAGE_EXTRACT_AND_RUN=1 ./tuic-proxy-gui-*.AppImage`。

AppImage 内已包含 WebKitGTK/GTK 运行库；目标机器需要 glibc ≥ 2.35（Ubuntu 22.04
基线）。

## 开发

```bash
cd apps/desktop
npm install
# 指向已构建的客户端，避免每次准备 sidecar：
TUIC_PROXY_CLIENT_BIN=/path/to/target/release/tuic-proxy-client npx tauri dev
```

`TUIC_PROXY_CLIENT_BIN` 覆盖 sidecar 路径，便于开发期直接跑 dev 模式。

Windows 10/11 自带 WebView2 运行时；Linux 需要系统 WebKitGTK 开发/运行库（构建
AppImage 时会打包）。
