# 上传二进制，一键部署

部署服务器无需 Rust、Cargo、源码或 Docker。安装包包含 musl 静态 Linux 二进制。
脚本支持 Ubuntu/Debian + systemd，服务默认监听 `0.0.0.0:25577`。
通过 `http://服务器IP:25577` 访问，脚本不修改系统防火墙或安全组。

## 生成安装包（二选一，在编译机完成）

### Windows：直接交叉编译

本机安装 Rust 和 Python 3。在项目目录执行：

```powershell
powershell -ExecutionPolicy Bypass -File scripts/build-linux.ps1
```

默认输出 `dist/skyblock-price-service-linux-amd64.tar.gz` 及 SHA256 校验文件。
脚本首次运行会把 cargo-zigbuild / Zig 安装到已忽略的 `target/cross-tools`，并通过 rustup 安装 Linux 目标。
随后直接交叉编译并打包，不需要 Docker 或 WSL。已缓存工具、目标和依赖时可加 `-Offline`。
ARM 服务器使用 `-Architecture arm64`。查看服务器 `uname -m`：`x86_64` 对应 amd64，`aarch64` 对应 arm64。

### GitHub Actions

将这个独立项目推到自己的 GitHub 仓库。在 Actions 中选择 **Build Linux package → Run workflow**。
完成后下载对应架构的 artifact，解开 GitHub 提供的 zip，取其中的 `.tar.gz` 安装包及 `.sha256` 文件。
工作流只编译并保存安装包，不发布 Release、不连接服务器。实际耗时、计费由 GitHub 账户决定。

Linux 编译机也可直接执行 `bash scripts/package-linux.sh`，需要 Rust 和 musl-tools。

## 上传、解压、安装

用 SFTP 上传 `.tar.gz` 和 `.sha256` 到服务器，进入上传目录执行：

```bash
sha256sum -c skyblock-price-service-linux-amd64.tar.gz.sha256
tar -xzf skyblock-price-service-linux-amd64.tar.gz
cd skyblock-price-service
sudo bash deploy/install.sh
```

ARM 服务器换用 arm64 文件。脚本完成后会输出监听地址和访问 token。

```bash
curl -H 'Authorization: Bearer 脚本输出的token' \
  'http://服务器IP:25577/v1/items/search?q=hyperion'
```

Java 调用这个地址，并携带相同的 Authorization 头即可。接口协议见 `api.md`。

## 更新和维护

若拍卖同步持续超时，可在解压的安装包内执行 `bash scripts/diagnose-upstream.sh`。
脚本只下载两页官方公开数据，输出 DNS、TLS、首字节、总耗时、传输速度和缓存头；不读取或输出 token，不修改服务。
每页最多 30 秒，临时文件退出时清理。低带宽或跨境链路波动需要结合服务器网络处理，增加重试无法提升实际下载速度。

上传新安装包，解压后再次执行相同安装命令。
脚本保留现有配置、token 和数据目录；使用新二进制重启服务。
若本地启动/鉴权检查失败，自动恢复上一个二进制。

```bash
sudo journalctl -u skyblock-price -f
sudo systemctl restart skyblock-price
sudo cat /etc/skyblock-price-service.env    # 找回访问 token
```

默认进程内存上限 512 MiB。监听地址由 `/etc/skyblock-price-service.toml` 的 `bind` 控制：

```toml
bind = "0.0.0.0:25577"
```

已有配置会保留；如使用旧端口，把该行改成上面的值并重启 `skyblock-price`。
拍卖分页默认 `auction_page_concurrency = 4`，旧配置无需补字段即可生效；所有线程共享原有请求间隔和限流退避。
脚本仅检查配置端口是否被其他进程占用。HTTP 直连不会加密 token，HTTPS 可通过已有代理自行配置。
首次同步完成前，`/health/ready` 返回 503，`/health/live` 和带认证的 `/v1/status` 可用于检查安装。

部署路径可通过环境变量配置，见 `sudo bash deploy/install.sh --help`。
使用 sudo 时通过 `sudo env PRICE_INSTALL_DIR=... PRICE_DEPLOY_DATA_DIR=... bash deploy/install.sh ...` 传入。
已有配置的 `data_directory` 必须与部署目录一致；其他配置不覆盖，部署服务直接读取配置中的监听地址。

来源：[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild)、[GitHub runner 架构说明](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)。
