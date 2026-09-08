# anureo ↔ anureo Server 功能验收

> 验证 workflow `dev-anureo` 产出的 5 个 P0 任务是否正常工作。
> 使用 Chrome DevTools MCP 直接调用 anureo API + 浏览器验证。

## 前提

- `anureo-server` 运行在 `http://127.0.0.1:3148`（固定端口便于前端 proxy）
- 无需 anureo 前端（直接 HTTP 验证后端 API）

## 验收清单

### OC-C: Provider/Model 数据 (bootstrap.rs)

| # | 方法 | 路径 | 期望 |
|---|------|------|------|
| C1 | GET | `/config/providers` | `{ providers: [...], default: {...} }`，providers 非空 |
| C2 | GET | `/provider` | `{ all: [...], connected: [...], default: {...} }`，all 非空 |
| C3 | GET | `/model` | 返回 model 列表（非空数组） |
| C4 | GET | `/api/model` | 同 C3（带前缀别名） |

**验证点**: provider 元素含 `id` + `name`，不含 `api_key`。

### OC-D: Route 别名 (routes.rs)

| # | 方法 | 路径 | 期望 |
|---|------|------|------|
| D1 | GET | `/model` | 200（非 404），返回 model 列表 |
| D2 | GET | `/api/model` | 200，同 D1 |

### OC-E: 文件操作 (fs.rs)

| # | 方法 | 路径 | Body | 期望 |
|---|------|------|------|------|
| E1 | POST | `/api/fs/mkdir` | `{ "path": ".verify-tmp" }` | 200, `{ path: ".verify-tmp" }` |
| E2 | POST | `/api/fs/write` | `{ "path": ".verify-tmp/hello.txt", "content": "hi" }` | 200, `{ path: "..." }` |
| E3 | GET | `/api/fs/stat?path=.verify-tmp/hello.txt` | — | 200, 含 size/is_file |
| E4 | POST | `/api/fs/rename` | `{ "from": ".verify-tmp/hello.txt", "to": ".verify-tmp/world.txt" }` | 200 |
| E5 | POST | `/api/fs/delete` | `{ "path": ".verify-tmp" }` | 200 |
| E6 | GET | `/api/fs/stat?path=.verify-tmp` | — | 404（已删除） |

### OC-F: Settings (settings.rs)

| # | 方法 | 路径 | 期望 |
|---|------|------|------|
| F1 | GET | `/config/settings` | 200, JSON 对象，不含 `api_key` |
| F2 | PUT | `/config/settings` | `{ "theme": "dark" }` | 200 |
| F3 | POST | `/config/reload` | — | 200 |

### OC-G: Git 操作 (git.rs)

| # | 方法 | 路径 | 期望 |
|---|------|------|------|
| G1 | GET | `/git/check` | 200, `{ is_repo: true }` |
| G2 | GET | `/git/status` | — (旧路由) 或 404 |
| G3 | GET | `/git/branches` | 200, 含 branch 列表 |
| G4 | GET | `/git/log` | 200, 含 commit 列表 |

## 验收结果 (2025-08-19)

后端: `anureo-server` release build, `http://127.0.0.1:3148`

### OC-C: Provider/Model 数据 ✅

| # | 状态 | 备注 |
|---|------|------|
| C1 | ✅ PASS | 6 providers, default=minimax-cn-coding-plan/MiniMax-M3 |
| C2 | ✅ PASS | all=6, connected=6, 无 api_key |
| C3 | ✅ PASS | `/model` 返回 12 个 model（含 location） |
| C4 | ✅ PASS | `/api/model` 同 C3 |

### OC-D: Route 别名 ✅

| # | 状态 | 备注 |
|---|------|------|
| D1 | ✅ PASS | `/model` → 200 |
| D2 | ✅ PASS | `/api/model` → 200 |

### OC-E: 文件操作 ✅

| # | 状态 | 备注 |
|---|------|------|
| E1 | ✅ PASS | mkdir `.verify-tmp` → 200 |
| E2 | ✅ PASS | write `hello.txt` → 200 |
| E3 | ✅ PASS | stat → `{ size: 2, isDir: false }` |
| E4 | ✅ PASS | rename → `{ from, to }` |
| E5 | ✅ PASS | delete → 200 |
| E6 | ✅ PASS | stat after delete → 404 |

### OC-F: Settings ⚠️

| # | 状态 | 备注 |
|---|------|------|
| F1 | ⚠️ PASS w/ issue | 返回完整 config JSON，**但 env 段泄露 API keys** |
| F2 | ✅ PASS | PUT 成功写入（确认 reviewer major finding：无白名单） |
| F3 | ✅ PASS | reload → `{ status: "ok", providers: 6 }` |

**安全问题**: `GET /config/settings` 的 env 段包含明文 `BIGMODEL_API_KEY`、`BOT_TOKEN`、`EXA_API_KEY`。需要在 settings.rs 中对 env 值做 mask 或过滤。

### OC-G: Git 操作 ✅

| # | 状态 | 备注 |
|---|------|------|
| G1 | ✅ PASS | `{ isRepo: true }` |
| G3 | ✅ PASS | 45 branches, current=feature/cli-server-backend |
| G4 | ✅ PASS | 20 commits, 最新 `2117e71` |

**小问题**: git branch 输出含 `%x1f` 控制字符（porcelain 格式分隔符），需要清理。

### 总结

| 任务 | 结果 | 遗留问题 |
|------|------|---------|
| OC-C | ✅ | — |
| OC-D | ✅ | — |
| OC-E | ✅ | — |
| OC-F | ⚠️ | env 段泄露 secrets; PUT 无白名单（reviewer 已标记） |
| OC-G | ✅ | branch 名称含控制字符 |

**整体**: 5/5 功能可用，2 个安全问题需后续修复。
