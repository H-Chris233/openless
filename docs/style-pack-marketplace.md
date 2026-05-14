# Style Pack Marketplace — 规划文档

**状态**：规划中（API 已预留 stub，未实装）
**起草日期**：2026-05-14
**owner**：待定

## 1. 目标

把现在「ZIP 包本地导入 / 导出」的体验扩展成一个公开的风格包市场：

- 用户可以把自己调好的风格包**上传**到云端，附带名称、描述、作者署名、标签、效果示例
- 其他用户可以**浏览 / 搜索 / 下载**别人的风格包，一键安装到本地
- 后期支持**版本升级提醒**、**收藏 / 评分**等基础社交属性

非目标（v1 不做）：
- 付费 / 抽成
- 风格包内嵌外部 prompt 注入 / 跨域 fetch（安全考虑，风格包始终是纯文本 prompt）
- 多人协作编辑 / fork

## 2. 架构概览

```
┌──────────────────┐        HTTPS         ┌─────────────────────┐
│  OpenLess client │ ◄──────────────────► │  marketplace API    │
│  (Tauri 2)       │      JSON over TLS   │  (TBD: Cloudflare   │
│                  │                       │   Workers / D1 /    │
│  Rust IPC →      │                       │   R2 for blobs)     │
│  reqwest client  │                       │                     │
└──────────────────┘                       └─────────────────────┘
        │                                            │
        │ local cache (~/Library/Application         │
        │ Support/OpenLess/market_cache/)            │
        ▼                                            ▼
   StylePackStore                                Postgres / D1
   (existing local                              listings + R2 blobs
    persistence layer)
```

**关键约束**：
- 客户端只能上传 / 下载 ZIP **bundle**（不直接传 JSON），保持跟现有 ZIP import/export 同构
- 服务端 ZIP 验证：解压后必须能反序列化成 `StylePack`、`prompt.chars().count() <= 50_000`、没有可执行附件
- 风格包 ID 上传后由服务端分配（`{author_slug}-{name_slug}-{version}`），跟本地 ID 解耦
- 客户端始终拿 ZIP 走现有 `import_style_pack_from_zip` 路径入库 —— 不另开一条「从市场直接写 Pack」的代码路径，避免双入口

## 3. HTTP API 规约

Base URL（待定）：`https://api.openless.app/v1/marketplace/`

所有响应统一信封：
```json
{
  "ok": true,
  "data": <T> | null,
  "error": null | { "code": "ERR_XXX", "message": "..." }
}
```

### 3.1 GET `/packs` — 列表 / 搜索

Query：
| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `q` | string | `""` | 关键词（名称 / 描述 / 标签） |
| `tag` | string | `""` | 单标签筛选 |
| `sort` | `recent` \| `popular` \| `name` | `recent` | 排序 |
| `cursor` | string | `null` | 分页游标 |
| `limit` | int (1-100) | `20` | 每页条数 |

Response data：
```typescript
{
  packs: MarketPackListing[];
  next_cursor: string | null;
}
```

`MarketPackListing`：
```typescript
{
  id: string;               // server-assigned, e.g. "alice-formal-v2.1"
  name: string;
  description: string;
  author: string;
  version: string;          // semver
  tags: string[];
  base_mode: "raw" | "light" | "structured" | "professional";
  recommended_model: string | null;
  compatible_app_version: string | null;
  downloads: number;
  rating_avg: number | null;
  rating_count: number;
  updated_at: string;       // ISO8601
  zip_size_bytes: number;
  zip_sha256: string;       // 客户端下载后校验
}
```

### 3.2 GET `/packs/{id}` — 详情

Response data：`MarketPackListing` + 额外字段：
```typescript
{
  ...listing,
  examples: StylePackExample[];   // 解压 ZIP 前的预览
  changelog: string | null;
  homepage_url: string | null;
}
```

### 3.3 GET `/packs/{id}/download` — 下载 ZIP

Response：`application/zip` 二进制流，带 `X-Pack-SHA256` header 用于校验。

服务端通过 redirect 直接指向 R2 / S3 预签 URL，避免代理流量。

### 3.4 POST `/packs` — 上传（需鉴权）

Headers：`Authorization: Bearer <api_key>`
Body：`multipart/form-data` with field `pack=@xxx.zip`

Response data：`MarketPackListing`（含新分配 id）

错误码：
- `ERR_INVALID_ZIP` — ZIP 解压失败 / 不是合法 StylePack JSON
- `ERR_PROMPT_TOO_LARGE` — prompt 字数超 50k
- `ERR_DUPLICATE_VERSION` — 同 author+name+version 已存在
- `ERR_RATE_LIMITED` — 触发限频

### 3.5 DELETE `/packs/{id}` — 撤回（需鉴权 + 必须是上传者）

### 3.6 POST `/packs/{id}/rate` — 评分（需鉴权）

Body：`{ score: 1..5, comment?: string }`

## 4. IPC 契约（Rust ↔ TS）

在 `src-tauri/src/commands.rs` 新增以下 stub（暂返回 `Err("not implemented yet")`，等服务端落地后实装）：

```rust
// 列表 / 搜索
#[tauri::command]
pub async fn market_list_packs(
    query: Option<String>,
    tag: Option<String>,
    sort: Option<String>,
    cursor: Option<String>,
    limit: Option<u32>,
) -> Result<MarketListResponse, String>;

// 详情
#[tauri::command]
pub async fn market_get_pack(id: String) -> Result<MarketPackDetail, String>;

// 下载 + 自动调用现有的 import_style_pack_from_zip 入库
#[tauri::command]
pub async fn market_download_pack(
    coord: CoordinatorState<'_>,
    app: AppHandle,
    id: String,
) -> Result<StylePack, String>;

// 上传（dirty 字段 = 已编辑、未保存）
#[tauri::command]
pub async fn market_upload_pack(
    coord: CoordinatorState<'_>,
    pack_id: String,
    api_key: String,
) -> Result<MarketPackListing, String>;

// 撤回
#[tauri::command]
pub async fn market_delete_pack(id: String, api_key: String) -> Result<(), String>;

// 评分
#[tauri::command]
pub async fn market_rate_pack(
    id: String,
    api_key: String,
    score: u8,
    comment: Option<String>,
) -> Result<(), String>;
```

DTO（在 `types.rs` 新增）：
```rust
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MarketPackListing {
    pub id: String,
    pub name: String,
    pub description: String,
    pub author: String,
    pub version: String,
    pub tags: Vec<String>,
    pub base_mode: PolishMode,
    pub recommended_model: Option<String>,
    pub compatible_app_version: Option<String>,
    pub downloads: u64,
    pub rating_avg: Option<f32>,
    pub rating_count: u32,
    pub updated_at: String,
    pub zip_size_bytes: u64,
    pub zip_sha256: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MarketPackDetail {
    #[serde(flatten)]
    pub listing: MarketPackListing,
    pub examples: Vec<StylePackExample>,
    pub changelog: Option<String>,
    pub homepage_url: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MarketListResponse {
    pub packs: Vec<MarketPackListing>,
    pub next_cursor: Option<String>,
}
```

TS wrappers（`src/lib/ipc.ts`）：
```typescript
export interface MarketPackListing { /* same shape */ }
export interface MarketPackDetail extends MarketPackListing { /* + examples, changelog, homepage_url */ }
export interface MarketListResponse { packs: MarketPackListing[]; next_cursor: string | null; }

export function marketListPacks(opts: {
  query?: string; tag?: string; sort?: 'recent' | 'popular' | 'name';
  cursor?: string; limit?: number;
}): Promise<MarketListResponse>;
export function marketGetPack(id: string): Promise<MarketPackDetail>;
export function marketDownloadPack(id: string): Promise<StylePack>;
export function marketUploadPack(packId: string, apiKey: string): Promise<MarketPackListing>;
export function marketDeletePack(id: string, apiKey: string): Promise<void>;
export function marketRatePack(id: string, apiKey: string, score: number, comment?: string): Promise<void>;
```

## 5. 鉴权模型

**v1 简化方案**：
- 用户在设置页输入个人 API key（服务端发放）
- API key 存到 OS Keychain，账户名 `com.openless.app.market_api_key`
- 客户端在 Header 加 `Authorization: Bearer <key>`
- 服务端校验 + 限频（每小时 60 次写、600 次读）

**v2 升级路径**（暂不做）：
- OAuth via GitHub / Google
- 上传时自动签名 ZIP，下载端校验签名

## 6. 缓存与版本检查

本地缓存目录：`<app_data>/market_cache/`
- `listings.json` — 上次拉的 listings（带 ETag）
- `packs/{id}.zip` — 已下载的 ZIP（按需保留，30 天自动清理）

版本升级提示：
- 启动时（带 dev-cap 24h 节流）调用 `/packs?ids=<已安装的 market_id...>` 拉对比
- 本地包记录 `installed_market_id` 和 `installed_market_version` 字段，新建 `StylePack` 时填，本地从 ZIP 安装也填
- 发现新版本 → 在 Style 页该包卡片角标显示 `New version: 2.3.0 →`

## 7. 客户端 UI 入口（v1 不做，先留位）

- Style 页头部加一个 tab：`本地 / 市场`
- 市场页：搜索栏 + tag 过滤 + 卡片列表 + 详情抽屉
- 上传：编辑某个本地包时，"导出 ZIP" 按钮旁边出现 "上传到市场"（需要先在设置里填 API key）

## 8. 安全 / 滥用对策

- ZIP 解压走 streaming，限制最大解压后大小 5 MB
- prompt 字段过滤明显的 prompt injection / 越狱（关键词预扫描 + 异步内容审核）
- 每用户每天上传上限 10 包，单包大小 ≤ 2 MB
- 上传后挂 24h 公开延迟（防恶意刷榜）

## 9. 实装 TODO（按优先级）

- [ ] 服务端选型（CF Workers + D1 + R2 vs Supabase vs 自托管 FastAPI）
- [ ] 服务端实装 + 部署环境（dev / staging / prod）
- [ ] 客户端 `types.rs` 加 DTO
- [ ] `commands.rs` 加 6 个 stub（**已完成**，返回 `not implemented yet`）
- [ ] `lib/ipc.ts` 加 wrapper（**已完成**）
- [ ] 实装 `market_download_pack`（先做单条路径打通：URL → 下载 → 走现有 import_style_pack_from_zip）
- [ ] 加凭据存储（Keychain 复用现有 `CredentialsVault`）
- [ ] UI：本地 / 市场 tab
- [ ] UI：搜索 + 卡片
- [ ] UI：详情面板
- [ ] UI：上传流程
- [ ] 升级提醒 badge
- [ ] 缓存清理 + ETag

## 10. 决策 / 风险记录

| 项 | 决策 | Why |
|---|---|---|
| ZIP 而非 JSON 上传 | 用 ZIP | 跟现有 import/export 同构；prompt 长文 + examples 用 ZIP 包压缩 |
| 服务端分配 ID | 是 | 防本地 ID 碰撞、用户重命名包不影响订阅 |
| 上传立刻可见 vs 审核 | 24h 公开延迟 | 防刷榜 + 给审核留空间 |
| API key vs OAuth | 先 API key | 简化 v1；登录态可 v2 升级 |
| 客户端缓存策略 | listings ETag + 已下载 ZIP 30 天 | 平衡流量和体验 |
| 国际化 / 跨境 | API 全英文 + 客户端 i18n | 服务端不存翻译，名称/描述支持任意 UTF-8 |
