# 上传二进制，用 screen 运行

部署服务器无需 Rust、Cargo、源码或 Docker。安装包包含 musl 静态 Linux 二进制。
解压后执行 `sh run.sh`，程序在前台运行，默认监听 `0.0.0.0:25577`。
不安装系统服务，不需要 root；用 screen 保持终端会话。
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

## 上传、解压、启动

用 SFTP 上传 `.tar.gz` 和 `.sha256` 到服务器，进入上传目录执行：

```bash
sha256sum -c skyblock-price-service-linux-amd64.tar.gz.sha256
tar -xzf skyblock-price-service-linux-amd64.tar.gz
cd skyblock-price-service
screen -S skyblock-price
sh run.sh
```

ARM 服务器换用 arm64 文件。如果未安装 screen，Ubuntu/Debian 可执行 `sudo apt-get install screen`。
首次启动会在解压目录创建 `config.toml` 和 `.env`，数据默认写入该目录下的 `data/`。
`.env` 保存自动生成的访问 token，权限为 0600；用 `cat .env` 查看，Java 端填写相同的 token。
已有文件不会覆盖，之后重启继续使用相同 token。也可提前自行创建 `.env`，内容为
`PRICE_API_TOKEN=你已有的token`（24–256 个无空白 ASCII 字符）。外部环境中的非空 `PRICE_API_TOKEN` 优先。

```bash
curl -H 'Authorization: Bearer .env中的token' \
  'http://服务器IP:25577/v1/items/search?q=hyperion'
```

Java 调用这个地址，并携带相同的 Authorization 头即可。接口协议见 `api.md`。

- **离开并保持运行：** 按 `Ctrl+A`，松开后按 `D`。
- **返回控制台：** `screen -r skyblock-price`。
- **停止：** 返回控制台后按 `Ctrl+C`，程序正常退出。
- **重新启动：** 在解压目录再次执行 `sh run.sh`。
- **查看会话：** `screen -ls`。

日志直接输出到 screen 控制台。启动脚本不后台化、不自动重启、不设置开机自启；服务器重启后需重新启动。

## 从旧 systemd 部署切换

旧服务只需停用一次，避免继续占用端口：

```bash
sudo systemctl disable --now skyblock-price
```

在新解压目录、首次启动前复制原来的 token，这样 Java 端不需要更换凭据：

```bash
umask 077
sudo cat /etc/skyblock-price-service.env > .env
```

默认创建本地配置并重新同步数据。若要保留原来的自定义配置和缓存，可在启动前复制：

```bash
sudo cat /etc/skyblock-price-service.toml > config.toml
sudo cp -a /var/lib/skyblock-price-service ./data
sudo chown -R "$(id -u):$(id -g)" ./data
```

然后把 `config.toml` 中的 `data_directory` 改为 `"data"`，再执行 `sh run.sh`。

## 更新和维护

若拍卖同步持续超时，可在解压的安装包内执行 `bash scripts/diagnose-upstream.sh`。
脚本只下载两页官方公开数据，输出 DNS、TLS、首字节、总耗时、传输速度和缓存头；不读取或输出 token，不修改服务。
每页最多 30 秒，临时文件退出时清理。低带宽或跨境链路波动需要结合服务器网络处理，增加重试无法提升实际下载速度。

更新时先在 screen 中按 `Ctrl+C` 停止程序，在原来的目录上解压新包，再运行 `sh run.sh`。
安装包不包含 `config.toml`、`.env` 和 `data/`，所以更新保留配置、token 和快照。
务必先停止再覆盖二进制；手动运行不提供自动回滚。

```bash
screen -r skyblock-price
# Ctrl+C 停止；在解压目录运行：
sh run.sh
```

监听地址由解压目录 `config.toml` 的 `bind` 控制：

```toml
bind = "0.0.0.0:25577"
```

已有配置会保留；修改后用 `Ctrl+C` 停止，再执行 `sh run.sh` 生效。
拍卖分页默认 `auction_page_concurrency = 4`，旧配置无需补字段即可生效；所有线程共享原有请求间隔和限流退避。
程序启动时绑定端口，端口已占用会报错退出。HTTP 直连不会加密 token，HTTPS 可通过已有代理自行配置。
首次同步完成前，`/health/ready` 返回 503，`/health/live` 和带认证的 `/v1/status` 可用于检查安装。

可通过 `PRICE_CONFIG` 指定其他配置文件，`PRICE_BIND` 覆盖监听地址。
脚本固定以解压目录作为工作目录，相对数据路径按该目录解析。
screen 运行不会继承旧 systemd 的 512 MiB 内存硬限制；应用自身的各项容量限制继续生效。

来源：[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild)、[GitHub runner 架构说明](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)。
