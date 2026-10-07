# API v1

除 `/health/live`、`/health/ready` 外，所有已注册接口必须有
`Authorization: Bearer <PRICE_API_TOKEN>`。JSON 统一封装：

```json
{"data": {}, "message": "ok", "status": 200, "timestamp": 1790000000000}
```

所有时间戳是 Unix 毫秒，价格单位 coins。错误的真实 HTTP 状态与 `status` 一致，另有 `code`。
价格、计数缺失不能解释成 0。接口不调用上游，而是读取当前快照。

## GET /v1/items/search

| 参数 | 规则 |
| --- | --- |
| `q` | 必填，1–128 字节；不区分英文大小写，空白拆词，全部词都要匹配 |
| `limit` | 1–50，默认 20 |
| `cursor` | 上一页 `next_cursor`，必须 URL 编码；快照变化，或价格筛选集合因到期变化时返回 409，重新查询第一页 |
| `market` | 可选 `bazaar` / `auction` / `npc`，只返回该市场有价格的项 |
| `only_priced` | 默认 false；true 过滤没有价格的项 |

合并官方目录、Bazaar 产品和拍卖分组。按完整 `key` 字典序，精确匹配也遵循同一顺序。
`total` 是本轮快照下的匹配数，不限制在默认 5 条。没有匹配时返回 200 和空列表。

```json
{
  "data": {
    "query": "hyperion",
    "total": 1,
    "items": [{
      "key": "HYPERION", "id": "HYPERION", "name": "Hyperion", "tier": "LEGENDARY",
      "variant": null, "icon_key": "HYPERION",
      "icon": {"material": "IRON_SWORD", "item_model": "hypixel_skyblock:item/uncategorized/hyperion", "glowing": false},
      "npc_sell_price": null, "bazaar": null,
      "auction": {"lowest_bin": 500000000, "listing_price": 500000000, "quantity": 1,
        "auction_uuid": "example-only", "ends_at": 1790001000000, "listings": 20, "pet_experience": null},
      "price_basis": "lowest_bin_per_unit_no_upgrade_valuation"
    }],
    "next_cursor": null,
    "sources": {
      "source": "Hypixel official API", "catalog_last_updated": 1790000000000,
      "bazaar": {"available": true, "last_updated": 1790000000000, "fetched_at": 1790000005000, "age_seconds": 10, "stale": false},
      "auctions": {"available": true, "last_updated": 1790000000000, "fetched_at": 1790000009000, "age_seconds": 10, "stale": false},
      "skipped_auctions_without_id": 0,
      "skipped_auctions_invalid": 0
    }
  },
  "message": "ok", "status": 200, "timestamp": 1790000010000
}
```

示例价格、时间和 UUID 均为虚构。名称只使用官方物品目录中的非空名称；缺失时显示通过格式校验的物品 ID，
不使用拍卖展示名称。ID 校验要求非空、最多 128 字节，且仅含 ASCII 字母、数字、下划线、冒号、连字符或句点；
异常 ID 的名称兜底为固定文本 `Unknown Item`。此规则同时适用于搜索和单物品价格接口，以及恢复的旧快照。
`icon` 在既无目录元数据、也无头颅或附魔书元数据时为 null。
`icon_key` 等于完整 `key`；宠物皮肤由最低挂牌物品的 NBT 提取，只返回 Mojang hash，不联网下载图片。

`sources.skipped_auctions_invalid` 是当前已发布拍卖快照中因单件 NBT/身份校验失败、非法挂牌价格或
分组 key 超限而排除的有效期内 BIN 数量，可能导致报价遗漏这些挂牌；不是历史累计值。
缺少物品 ID 单独计入 `skipped_auctions_without_id`。这些计数也出现在 `/v1/status` 的 `sources` 中。
页面 JSON、HTTP 或快照一致性失败仍拒绝发布，不计入单件跳过数。

Bazaar 返回 `instant_buy`、`instant_sell`、`buy_order`、`sell_offer`、`buy_volume`、`sell_volume`。
这些是当前最优盘口；buy order / sell offer 字段是参考盘口价，不保证立刻成交，也不包含税。

宠物按类型/品质/皮肤分组，`price_basis` 为 `lowest_bin_per_unit_across_pet_levels`。
书按全部附魔及等级分组。`variant` 给出结构化字段，`key` 为完整分组标识，不要手工拼接。

## GET /v1/items/{id}/price

返回 `data.item` 和 `data.sources`，结构同搜索。
可用 `?variant=<完整搜索结果key>` 查具体分组，例如对 `PET` 查询宠物分组；参数必须 URL 编码。
不存在返回 404。不指定变体时只查询同名基础 ID，不会把某个宠物/附魔书组冒充基础项。

## 运行状态

- `GET /health/live`：存活时 200，停止中 503；不因上游失败而变成死亡检查。
- `GET /health/ready`：有目录且 Bazaar/拍卖都未 stale 时 200，否则 503。
- `GET /v1/status`：来源时间、条目数、最近同步结果、运行时间和主要预算。
  `limits` 同时返回当前生效的 `request_timeout_secs`、`response_timeout_secs`、
  `request_spacing_ms`、`request_retries`、`auction_page_concurrency` 和 `round_timeout_secs`，方便核实旧配置采用的默认值。
- `GET /metrics`：Prometheus 文本，含请求/429/同步/换版/落盘失败计数、快照年龄、条目数及 Linux RSS。

## 失败与过期

- 400：参数、ID、limit 或 cursor 格式错误。
- 401：认证失败。
- 404：物品/分组/接口不存在。
- 409：分页期间快照变更，重新查询第一页。
- 429：超过配置的同时处理请求数量；返回 `Retry-After: 1`，不无界排队。
- 503：目录尚未加载，或 readiness 尚不满足。某市场尚未加载不阻止其他市场/物品的查询。
- 414：请求 URI 超过 2048 字节。

上游失败由后台处理，不把旧数据标记为新数据。`stale` 看官方 `last_updated`，
`available` 还受 `max_price_age_secs` 限制；不可用市场价格为 null，仍保留状态信息。
最低 BIN 到期后也为 null，即便快照整体尚未过期。应在渲染时显示来源时间，
对过期数据给出明确状态，不能显示成刚实时查到的价格。
