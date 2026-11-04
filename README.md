# tuic-proxy

基于 QUIC 的加密 SOCKS5 代理。客户端在本地（或局域网）提供 SOCKS5 入口，把请求
通过一条经过 **PSK 双向认证** 的 QUIC/TLS 1.3 链路转发给服务端；服务端做 DNS
解析、连接目标，并把响应原路返回。

```text
局域网电脑 ──SOCKS5──> tuic-proxy-client ──QUIC / TLS 1.3 + PSK──> tuic-proxy-server ──TCP──> 目标
```

两端只要共享同一个 PSK 就能建立加密链路：不需要证书、域名或额外的配置文件。

- 服务端：单文件 Rust 程序，可打成 `.deb` 用 systemd 托管。
- 客户端：同一个 crate 里的命令行程序，另有 **双击即用的 Windows / Linux 图形客户端**。

## 目录

- [特性](#特性)
- [快速开始](#快速开始)
- [参数](#参数)
- [桌面 GUI 客户端](#桌面-gui-客户端)
- [工作流程](#工作流程)
- [安全设计](#安全设计)
- [构建](#构建)
- [测试](#测试)
- [打包与发布](#打包与发布)
- [代码结构](#代码结构)
- [当前限制](#当前限制)

## 特性

- **全程加密**：底层是 QUIC/TLS 1.3，所有载荷由 TLS 加密；PSK 不直接参与数据加密。
- **PSK 认证**：两端用同一个 PSK 做 HMAC 双向认证，证明绑定到当前 TLS 会话。
- **原生多路复用**：一条 QUIC 连接可同时承载多个 SOCKS5 连接（每条连接一个双向流），
  无需为每个请求重复握手。
- **连接池**：流按轮询分散到多条 QUIC 连接（默认 4 条）。单条 QUIC 连接只有一个
  拥塞窗口，高 RTT 链路上会成为瓶颈；多条连接各有独立窗口，并发时能叠加带宽。
- **服务端 DNS**：客户端把域名原样发给服务端解析，避免本地 DNS 泄露。
- **安全默认值**：默认拒绝回环、私网、链路本地地址，避免服务端被当作内网跳板。
- **图形客户端**：Windows 单文件 `.exe` 与 Linux AppImage，双击运行、免安装；配置保存在本机。

## 快速开始

先生成一个高熵 PSK（32 字节随机数的十六进制形式）：

```console
openssl rand -hex 32
```

**服务端**（假设公网 IP 为 `203.0.113.10`，UDP 4433 已放行）：

```console
./tuic-proxy-server --psk '<同一个字符串>'

# 或从文件读取，避免密钥出现在进程列表/历史里（路径前加 @）：
./tuic-proxy-server --psk @/etc/tuic-proxy/psk
```

监听地址默认 `0.0.0.0:4433`，一般无需指定；换端口再加 `--listen`。

**客户端**（运行在内网机器上，其他电脑通过 `192.168.1.20:1080` 使用）：

```console
./tuic-proxy-client --server 203.0.113.10 --psk '<同一个字符串>' --socks 0.0.0.0:1080
```

`--server` 省略端口时默认 4433；`--socks` 省略时默认 `127.0.0.1:1080`，也可以只写
端口（如 `--socks 1080`）。内网其他电脑的浏览器/系统代理填写
`SOCKS5 192.168.1.20:1080`，并选择“SOCKS5 代理时进行远程 DNS 解析”。命令行验证：

```console
curl --socks5-hostname 127.0.0.1:1080 https://example.com
```

> 监听非回环地址（如 `0.0.0.0:1080`）时，同网段任何人都能使用这条代理；请仅在可信
> 网络中这样配置。

IPv6 服务端写成 `--server '[2001:db8::1]'`（可带 `:端口`）即可，客户端会自动选择
对应地址族的本地 UDP 连接。

## 参数

只有标 **必填** 的参数通常需要给出，其余都有合理默认值。

| 程序 | 参数 | 说明 |
| --- | --- | --- |
| 两者 | `--psk SECRET\|@FILE` | **必填**。共享密钥，两端必须一致；`@PATH` 从文件读取 |
| server | `--listen ADDR` | UDP 监听地址，默认 `0.0.0.0:4433` |
| server | `--allow-private` | 允许访问私网/回环目标（测试或内网穿透时需要） |
| client | `--server HOST[:PORT]` | **必填**。服务端地址，端口默认 4433 |
| client | `--socks ADDR\|PORT` | SOCKS5 监听，默认 `127.0.0.1:1080` |

其余行为使用固定默认值：每条 QUIC 连接最多 1024 条流、空闲超时 60 秒、客户端使用
4 条 QUIC 连接的连接池、优先 IPv4。日志级别用 `RUST_LOG` 控制（默认 `info`），例如
`RUST_LOG=tuic_proxy=debug` 会输出 QUIC 路径统计。

## 桌面 GUI 客户端

`apps/desktop` 是一个 Tauri 2 桌面壳（Rust + 系统 WebView），真正跑隧道的仍是
`tuic-proxy-client`（以 sidecar 子进程启动）。发布物都是**单文件、双击即用、免安装**：

| 平台 | 发布物 | 说明 |
| --- | --- | --- |
| Windows 10/11 | `tuic-proxy-client-gui-<版本>-windows-amd64.exe` | 单个 exe，内含 GUI + sidecar |
| Linux x86_64 | `tuic-proxy-client-gui-<版本>-linux-amd64.AppImage` | 单个 AppImage，内含 GUI、sidecar 与 WebKitGTK 依赖 |

- Windows 首次运行把 `tuic-proxy-client-gui.exe` + `tuic-proxy-client.exe` 解到
  `%LOCALAPPDATA%\TUICProxy\bin\<哈希>\`；Linux AppImage 直接挂载运行，不写系统目录；
- 界面提供连接参数（服务端、PSK、本地 SOCKS5 端口）、启动/停止、运行状态与日志；
- “链路测试”页经本地 SOCKS5 访问 Cloudflare 测速端点，显示延迟/抖动、下载/上传
  速率，并汇总 QUIC 路径统计给出隧道丢包率与平均 RTT；
- 配置存在 WebView 的 localStorage，升级不丢；
- 最小化或关窗时收进系统托盘，代理继续运行；左键点击托盘图标恢复窗口，托盘菜单
  “退出”才会结束 sidecar（Windows 用 Job Object，Linux 用 SIGTERM），不残留占用端口。

Linux 下载后需要一次 `chmod +x tuic-proxy-client-gui-*.AppImage`（浏览器下载会丢掉可执行位），
之后即可双击运行；若系统没有 FUSE 2，可用
`APPIMAGE_EXTRACT_AND_RUN=1 ./tuic-proxy-client-gui-*.AppImage`。

构建方法见 [`apps/desktop/README.md`](apps/desktop/README.md)。

## 工作流程

1. 局域网电脑向客户端发起 SOCKS5 `CONNECT`（IPv4、IPv6 或域名）。
2. 客户端完成 SOCKS5 协商，把目标地址编码后写入一条新的 QUIC 双向流。
3. 服务端读取目标地址，按需做 DNS 解析并连接目标。
4. 服务端把状态字节写回流；失败时客户端据此返回对应的 SOCKS5 错误码。
5. 成功后双向转发字节，QUIC 流的半关闭映射为 TCP 的 FIN/`shutdown()`。
6. 所有 SOCKS5 连接共用已认证的 QUIC 连接池；某条连接失效时按需自动重连。

## 安全设计

- **加密**：QUIC 强制 TLS 1.3，载荷始终加密；PSK 不直接参与数据加密，避免自行设计
  分组密码模式带来的 nonce 复用风险。
- **身份认证**：服务端启动时生成临时自签名证书，客户端不校验证书链；真正的身份来自
  PSK 握手。双方的 HMAC 输入包含 TLS exporter 值，因此一个同时终结两条 TLS 链路的
  中间人无法把一端的证明转发到另一端。
- **重放**：TLS exporter 每条连接都不同，旧连接的认证证明无法用于新连接。
- **PSK 强度**：PSK 经 HKDF 拉伸为 256 位密钥，但安全性仍取决于原始字符串的熵；请用
  `openssl rand -hex 32` 生成。少于 16 字节会被拒绝，少于 32 字节会告警。
- **PSK 的传递方式**：`--psk <密钥>` 会出现在进程列表和 shell 历史中；长期运行的服务
  建议写成 `--psk @/etc/tuic-proxy/psk`（从文件读取），并把文件权限设为 `600`。
- **出站限制**：默认拒绝回环、RFC 1918 私网、链路本地、CGNAT 等目标；确需访问内网时
  显式加 `--allow-private`。
- **未实现 0-RTT**：不启用 QUIC 早数据，避免重放语义带来的复杂度。

## 构建

需要 Rust 1.85+（stable）。仓库根是单个 Cargo 包；桌面 GUI 有自己的 Tauri 工程，
不参与根 `cargo build`。

```console
cargo build --release
```

产物：`target/release/tuic-proxy-server`、`target/release/tuic-proxy-client`（Windows 上为 `.exe`）。

> 本机开发环境把 Rust 装在 `D:\rust`，如果 `cargo` 不在 PATH：
> ```console
> export RUSTUP_HOME='D:\rust\rustup'
> export CARGO_HOME='D:\rust\cargo'
> export PATH="/d/rust/cargo/bin:$PATH"
> ```

## 测试

```console
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

端到端测试覆盖：IPv4 目标、服务端域名解析、32 条并发流共享一条 QUIC 连接、2 MiB
大载荷、错误 PSK 被拒绝、连接池按轮询分配。所有网络测试只用回环地址与临时端口。

## 打包与发布

Linux 服务端打成 Debian/Ubuntu 的 `.deb`。打包器是纯 Python 标准库脚本
（`scripts/build_deb.py`），不需要 `dpkg-deb`/`fakeroot`/`tar`，Windows 和 Linux
上跑法一致。包内容：

```text
/usr/bin/tuic-proxy-server
/usr/bin/tuic-proxy-client
/usr/lib/systemd/system/tuic-proxy.service
/etc/tuic-proxy/server.env     (dpkg conffile，0600)
/usr/share/doc/tuic-proxy/
```

本地打包（需先构建出 Linux 二进制）：

```console
cargo build --release
python3 scripts/build_deb.py --bin-dir target/release --out-dir dist
python3 scripts/build_deb.py --self-test      # 只跑打包器自检
```

安装后先编辑 `/etc/tuic-proxy/server.env`，填入 `PSK_ARGS=--psk <密钥>`，再
`systemctl enable --now tuic-proxy`。安装时不会自动启动——包里不含密钥。

发布由 GitHub Actions 完成（`.github/workflows/release.yml`）：推送形如 `v0.0.1` 的
tag 即触发整条流水线，并行构建三类发布物：

| 产物 | 内容 |
| --- | --- |
| `tuic-proxy-server-<版本>-linux-<架构>.deb` | 服务端 + 命令行客户端 + systemd 单元（Ubuntu 22.04 构建） |
| `tuic-proxy-client-gui-<版本>-linux-<架构>.AppImage` | Linux GUI，免安装 |
| `tuic-proxy-client-gui-<版本>-windows-amd64.exe` | Windows GUI，单文件 |

deb 任务同时跑 `cargo test` 门禁，并做 `dpkg -i`、版本/文件/conffile 校验、systemd
起停与卸载冒烟。全部通过后创建 GitHub Release，附上三个产物与 `SHA256SUMS`。tag
必须与 `Cargo.toml` 和 `apps/desktop/src-tauri/tauri.conf.json` 的 `version` 一致，
否则 workflow 直接失败。

```console
git tag v0.0.1 && git push origin v0.0.1
```

发布前如需换仓库地址或维护者，请修改 `packaging/debian/metadata.env`。

## 代码结构

```text
src/
  lib.rs       公共常量、模块声明
  auth.rs      PSK 派生、HMAC 双向认证、exporter 绑定
  tls.rs       QUIC 的 rustls 配置、临时自签名证书
  target.rs    目标地址编码/解析、私网地址判定
  socks5.rs    SOCKS5 协商、请求解析
  relay.rs     TCP <-> QUIC 流双向转发
  tunnel.rs    客户端连接池、按需重连
  client.rs    客户端启动、SOCKS5 accept loop、服务端地址解析
  server.rs    服务端启动、认证、DNS、出站连接
  bin/         tuic-proxy-server / tuic-proxy-client
tests/
  e2e.rs       真实 QUIC 回环端到端测试
apps/desktop/  Tauri 2 图形客户端（sidecar + 单文件引导器）
packaging/     deb 元数据、systemd 单元
scripts/       Python deb 打包器
```

本项目直接依赖 `quinn` / `rustls`，**不依赖也不修改 TUIC 或 Wind 的源码**，因此上游
库更新时只需在本项目内适配 API，不会与库代码产生冲突。

## 当前限制

- 只实现 SOCKS5 `CONNECT`（TCP）；UDP ASSOCIATE 尚未实现。
- 所有客户端共享同一个 PSK，无法区分用户；需要按用户限速/审计时可扩展为多 PSK。
- 断线时已有的 SOCKS5 连接会关闭，不做传输层连接迁移。
- 桌面 GUI 提供 Windows 单文件与 Linux AppImage；macOS 需自行用 Tauri 构建。
