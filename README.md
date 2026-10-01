# SkyBlock Price Service

独立 Rust 项目与独立 Git 仓库。只使用 Hypixel 官方物品、Bazaar、拍卖 API；
不依赖外层 Java 项目的源码、构建产物或第三方价格服务。Java 后端只需调用查询 API，
图片排版和静态图标仍由 Java 完成。

## 启动

本机源码运行需要 Rust 1.85 或以上。远程服务器使用下方的预编译安装包部署。

```sh
cargo build --locked --release
cp config.example.toml config.toml
export PRICE_API_TOKEN="$(openssl rand -hex 32)"
./target/release/skyblock-price-service
```

`PRICE_CONFIG` 指定配置文件；默认读取工作目录的 `config.toml`，不存在时使用默认值。
`PRICE_BIND` 可覆盖监听地址，默认 `0.0.0.0:25577`。
查询接口必须使用 `Authorization: Bearer $PRICE_API_TOKEN`，没有有效 token 时服务拒绝启动。
这些公开上游端点不需要 Hypixel 玩家查询 API key。

```sh
curl -H "Authorization: Bearer $PRICE_API_TOKEN" \
  'http://127.0.0.1:25577/v1/items/search?q=diamond&limit=20'
curl -H "Authorization: Bearer $PRICE_API_TOKEN" \
  'http://127.0.0.1:25577/v1/items/HYPERION/price'
curl 'http://127.0.0.1:25577/health/ready'
```

## 同步与内存边界

- 一个同步协调线程，拍卖分页默认最多 4 个工作线程并发；共享连接池、请求间隔和 Retry-After。
  固定延迟从本轮结束开始计算，无重叠轮次、无无限任务队列；`auction_page_concurrency` 可设为 1–8。
- 物品目录每 6 小时、Bazaar 每 60 秒、拍卖每 90 秒，均可配置。
  三类任务由同一协调线程调度；拍卖每次只下载一波工作线程能处理的缺页，然后交还调度，避免一口气占满整轮 180 秒。
  缺页轮转选取，低页码持续返回旧缓存或超时不会挡住后面的页；还有未尝试的缺页时 1 秒后继续，否则 5 秒后重试。
- HTTP JSON 流式读取、逐记录反序列化；未知字段跳过。NBT 只提取 ID、数量、宠物、附魔书分组和头颅纹理 hash。
- 默认解压后的 HTTP 响应最多 16 MiB；NBT 编码最多 64 KiB、解压最多 256 KiB，另有深度、节点、数组和字符串限制。
- 默认目录最多 15,000 项、Bazaar 10,000 项、拍卖 200 页 / 200,000 条，价格汇总最多 50,000 项。
- 不缓存原始拍卖 JSON、Lore、完整 NBT、皮肤 base64、历史成交明细或用户查询结果。
- 同步时持有已发布摘要、最多六个未完成批次的摘要，以及默认最多 4 页的临时价格摘要和流式缓冲。
  未完成批次的条目总数共用 `max_price_entries` 上限，不是每批各占一份预算；超过批次数或总条目预算时淘汰最旧批次。
  CDN 的分页缓存可能落后数分钟，因此保留多批次以接纳迟到页；不积攒分页原始 JSON。
  查询只生成最多 50 项；有序索引合并不建立全部匹配结果列表。
- 每页必须属于同一 `lastUpdated`、`totalPages`、`totalAuctions`；检查完整页数和记录数。
  分页按时间戳进入各自的批次，跨轮保留已完成页；重复页不会重复计数，落后页不会覆盖新批次。
  批次未收齐时 5 秒后仅补最新批次的缺页，不再因单页换版清空整轮进度。
  收齐后检查总记录数并原子发布，不发布部分分页或混合批次；也不额外检查首页而丢弃完整快照。
  网络/解析失败时保留此前已验证的分页并指数退避；合并超预算则丢弃该未完成批次，已发布快照不变。
  所有补页和退避都遵守同一全局请求间隔和 Retry-After。`sync pending` 日志表示补页中，并非进程崩溃。
- 上游默认请求间隔 500 ms，单次网络等待超时 15 秒，最多额外重试 2 次。429/5xx 退避；支持两种 `Retry-After`。
  `response_timeout_secs` 默认 60 秒，为单页请求、重试、流式读取和解析设置共享截止时间，避免零星到达的数据不断延续读取。
  截止时间在读取之间检查，正在阻塞的单次读取可能额外等待至 `request_timeout_secs`；不会增加无界后台下载任务。
  慢页日志包含路径、耗时、已解压字节数，失败日志保留完整错误链。此限制不能提升服务器到上游的实际带宽。
  较长冷却会跨任务保留，不能绕过限流。上游没有承诺此配置永远不会触发限流。
- `ArcSwap` 原子发布。三个市场各自有时间戳，不能把 Bazaar 和拍卖伪装成同一时刻的数据。
- 每个来源保留一份有版本、字节上限的 JSON 磁盘快照；原子替换，重启校验后恢复。
  一个数据目录只能被一个进程锁定。磁盘保存失败会记录指标，内存中已验证的新快照仍可用。
- 价格超过 300 秒标为 stale，超过 1 小时不再返回该市场的价格；时间阈值可配置。
  最低拍卖已到期时返回 null，而不猜测另一条价格。物品信息和 NPC 价格仍可查询。

这些限制控制工作量，不等价于经过测量的 RSS 硬上限。Linux 部署用进程内存上限隔离资源；
先用 `/metrics` 和一次完整同步测量，再调整预算。依赖和 TLS 运行时也占用内存。

## 价格口径

- Bazaar 用最优盘口：`buy_summary` 的最低 ask 为 instant buy；
  `sell_summary` 的最高 bid 为 instant sell。不是 `quick_status` 的前 2% 加权均价。
- BIN 按单件价格比较，同时返回整笔挂牌价、数量、拍卖 UUID、到期时间。
  一组价格是抓取时可见记录的最低值，不保证用户打开拍卖时尚未被购买。
- 普通物品按 SkyBlock ID 汇总，不进行附魔、星级、强化、颜色或重铸的价值估算。
- 附魔书按完整附魔名称/等级集合分组；多附魔书不会冒充单附魔书。
- 宠物按类型、品质、皮肤分组，最低价跨等级；返回最低挂牌宠物的经验 `pet_experience`。
  本版不把经验猜成等级，不提供指定等级估价。
- 缺少 SkyBlock ID 的 BIN 不参与汇总，数量公开在 `sources.skipped_auctions_without_id`。
  解析错误不静默忽略，整轮失败。非 BIN 的竞拍出价不当作成交价。
- 缺失价格使用 null，不用 0 代替；没有上架、过期和市场尚未加载均可通过来源信息区分。
- 搜索支持英文名称、ID、附魔/宠物分组关键词，不自带中文翻译表。
- 图标返回材质、颜色、模型、Mojang texture hash，不下载/渲染图片。`icon_key` 使用完整分组 key，供本地素材映射；
  宠物和拍卖头颅补充 NBT 中的纹理 hash，附魔书补充原版材质。其他没有元数据的项 `icon` 为 null。

## Linux 部署

推荐使用 [上传二进制的一键部署](docs/deployment.md)：在 Windows 本机直接交叉编译，或通过 GitHub Actions 生成 Linux 安装包，
上传后执行 `sudo bash deploy/install.sh`，通过 `http://服务器IP:25577` 访问。
脚本安装预编译程序和 systemd 服务，保留 token、配置与快照，不修改系统防火墙。

可以用 systemd，示例在 [`deploy/skyblock-price.service`](deploy/skyblock-price.service)。
示例路径都需要按你的部署修改：

- 程序：`/opt/skyblock-price-service/skyblock-price-service`
- 配置：`/etc/skyblock-price-service.toml`，设置 `data_directory = "/var/lib/skyblock-price-service"`
- 环境文件：`/etc/skyblock-price-service.env`，仅含 `PRICE_API_TOKEN=...`，权限 0600
- 创建专用 `skyblock-price` 用户；systemd 的 `StateDirectory` 管理数据目录。
- 默认 `MemoryHigh=256M`、`MemoryMax=512M` 是预算，不是实测占用。
  应用超预算会被系统终止并重启，避免吃完宿主机内存。
- 监听地址由配置中的 `bind` 控制，默认 `0.0.0.0:25577`。HTTP 直连仍需 Bearer token；
  如需传输加密，可自行配置 HTTPS 反向代理。
- `/health/live` 不依赖上游，`/health/ready` 要求三类数据已加载且价格未过期。
  `/v1/status` 和 `/metrics` 需要认证。Linux `/metrics` 包含进程 RSS。

也提供 Dockerfile。在目标架构构建：

```sh
docker build -t skyblock-price-service .
docker run --name skyblock-price-service --restart unless-stopped \
  --memory=512m --cpus=2 --pids-limit=32 \
  -p 25577:25577 --env-file /etc/skyblock-price-service.env \
  -v skyblock-price-data:/app/data skyblock-price-service
```

本地检查：`cargo fmt --check`、`cargo clippy --locked -- -D warnings`、`cargo build --locked --release`。
不依赖生产玩家、数据库或真实审核数据。

## 本机实测

2026-10-01，Windows release、默认同步配置、官方公开 API：

- 最后一轮处理 42,155 条拍卖，汇总 2,594 个价格分组；成功轮次耗时约 22 秒。
- 目录 5,655 项，Bazaar 2,197 项；搜索、分页、价格查询和宠物 texture hash 已实际核对。
- 初次加载及携带旧快照刷新时，进程工作集峰值约 21.7 MiB，采样 private bytes 峰值约 13.4 MiB。
- 重启后切换到返回 503 的本地上游，快照恢复和旧价格查询正常。

这些是当前数据规模下的本机结果，不是 Linux RSS 或未来上限；目标 Linux 部署仍需观察 `/metrics`。
官方分页缓存可能换版，失败轮次会增加首次就绪等待时间，不能把 22 秒理解为每次启动保证。

## Git 与来源

`target/`、`data/`、实际配置、环境文件和日志已忽略；`Cargo.lock` 必须跟踪。
外层 Java 仓库忽略整个目录，在本目录独立执行 Git 操作。没有创建远程仓库或提交。

接口协议见 [`docs/api.md`](docs/api.md)。数据来源和字段说明来自
[Hypixel 官方 API](https://api.hypixel.net/)，使用须符合
[Hypixel API Policies](https://developer.hypixel.net/policies)。
本项目未复制第三方价格算法或第三方图片。依赖来源及版本记录在 `Cargo.lock`，
随归档提供的许可、版权原文及各包许可声明保存在 `THIRD_PARTY_NOTICES.txt`，分发时一并携带；Docker 镜像已包含该文件。
升级依赖后，先执行 `cargo fetch --locked`，再用 Python 3.11+ 执行
`python scripts/update_notices.py` 重新生成说明；该脚本只读取校验通过的本机 Cargo 源码归档。
