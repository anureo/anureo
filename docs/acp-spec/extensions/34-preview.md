# Preview（开发服务器预览代理）

> 命名空间: `_anureo.dev/preview/*`
> Capability key: `preview`

## Capability

```json
{
  "preview": {
    "proxy": true
  }
}
```

- 声明 `preview` capability 后，client 可以通过 server 代理访问本地开发服务器的预览输出。
- Preview Proxy **仅在 web/desktop 运行时可用**——VS Code 等扩展运行时不支持。
- Proxy 不暴露内部端口；URL 路径必须经 server 校验，**不允许任意 SSRF**。

### 安全模型

```
Client (browser)
  → Preview Proxy request (validated URL path)
  → Server-side allowlist check
  → Fetch from local dev server (localhost only)
  → Return response (filtered headers)
```

- Proxy **只允许代理 localhost 上的开发服务器**——不允许代理外部地址。
- URL 路径必须通过 server 端的 path validation：
  - 解析后的目标地址必须为 `127.0.0.1` 或 `::1`。
  - 端口必须在 server 允许的端口范围内（通常为 1024-65535，排除已知系统端口）。
  - 不允许 `file://`、`ftp://` 等非 HTTP/HTTPS 协议。
  - 不允许访问 server 自身的 ACP endpoint（防止递归）。
- Response headers 会被过滤——移除 `Set-Cookie`、`X-Forwarded-*` 等敏感头。

### 运行时可用性

| 运行时 | 支持 | 说明 |
|---|---|---|
| Web (browser) | ✅ | 主要使用场景，在 UI 内嵌入 iframe 预览 |
| Desktop (Electron/Tauri) | ✅ | 通过内置 proxy 访问本地 dev server |
| VS Code extension | ❌ | VS Code 有自己的 port forwarding 机制 |
| CLI/stdio | ❌ | 无 HTTP proxy 能力 |

---

## Methods

### `_anureo.dev/preview/proxy`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `preview.proxy` |
| 权限 | Server-side authorization（已认证连接即可） |
| 超时 | 建议 30s（单次代理请求） |

**Request:**

```json
{
  "method": "GET",
  "port": 3000,
  "path": "/dashboard",
  "headers": {
    "Accept": "text/html"
  },
  "body": null
}
```

**Response:**

```json
{
  "status": 200,
  "headers": {
    "Content-Type": "text/html; charset=utf-8"
  },
  "body": "<!DOCTYPE html><html>...</html>",
  "bodyEncoding": "utf-8",
  "fromCache": false
}
```

**二进制内容 Response（如图片）:**

```json
{
  "status": 200,
  "headers": {
    "Content-Type": "image/png"
  },
  "body": "data:image/png;base64,iVBORw0KGgo...",
  "bodyEncoding": "base64",
  "fromCache": false
}
```

**Rust 类型:**

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct PreviewProxyRequest {
    /// HTTP method: GET, POST, PUT, DELETE, HEAD, OPTIONS
    #[serde(default = "default_method")]
    pub method: String,
    /// Local dev server port (localhost only)
    pub port: u16,
    /// URL path (must start with /)
    #[serde(default = "default_path")]
    pub path: String,
    /// Request headers (filtered by server)
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Request body (string or null)
    #[serde(default)]
    pub body: Option<String>,
}

fn default_method() -> String { "GET".to_string() }
fn default_path() -> String { "/".to_string() }

#[derive(Debug, Clone, Serialize)]
pub struct PreviewProxyResponse {
    pub status: u16,
    /// Filtered response headers (sensitive headers removed)
    pub headers: HashMap<String, String>,
    /// Response body (text or base64-encoded binary)
    pub body: String,
    /// "utf-8" for text, "base64" for binary
    pub body_encoding: String,
    /// true if served from proxy cache
    pub from_cache: bool,
}

/// Ports blocked by the proxy (anti-SSRF)
pub const BLOCKED_PORTS: &[u16] = &[
    22,    // SSH
    25,    // SMTP
    445,   // SMB
    3306,  // MySQL
    5432,  // PostgreSQL
    6379,  // Redis
    27017, // MongoDB
];
```

**逻辑说明:**

1. Server 校验 `port` 是否在允许范围内且不在 `BLOCKED_PORTS` 列表中。
2. Server 构造目标 URL：`http://127.0.0.1:{port}{path}`。
3. Server 校验 `path` 不包含路径遍历（`../`）或协议注入（`://`）。
4. `method` 必须为安全 HTTP method（GET、POST、PUT、DELETE、HEAD、OPTIONS）。
5. Request headers 中 `Host`、`Origin` 等 proxy 控制头由 server 设置，client 传入的同名头被覆盖。
6. Server 执行 HTTP 请求到 localhost dev server，设置合理超时（默认 30s）。
7. Response headers 过滤：移除 `Set-Cookie`、`Transfer-Encoding`、`Connection`、`X-Powered-By` 等。
8. Body 编码：`Content-Type` 为 text/* 或 application/json 时使用 `utf-8`，否则使用 `base64`。
9. Server 可实现短期缓存（如 5 秒 TTL）用于静态资源，`fromCache` 标识是否命中缓存。
10. 如果 dev server 不可达（连接拒绝），返回 `proxy_target_unreachable` error。
11. **Proxy 不维护 WebSocket 连接**——仅支持 HTTP 请求/响应模式。如需 HMR WebSocket，client 应直接连接 dev server 的 WS 端口。

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `preview.proxy` 或运行时不支持 | initialize 未声明 / 非 web/desktop 运行时 |
| `forbidden` | 无权限 | server authorization 拒绝 |
| `invalid_params` | 参数校验失败 | `port` 在 BLOCKED_PORTS 中、`path` 包含非法字符、`method` 不支持 |
| `proxy_target_unreachable` | 目标 dev server 不可达 | localhost:{port} 连接拒绝 |
| `proxy_timeout` | 代理请求超时 | dev server 响应超时 |
| `ssrf_blocked` | SSRF 防护拦截 | 解析后的目标地址非 localhost |
| `internal_error` | Server 内部错误 | 代理过程异常 |

### SSRF 防护细则

| 检查项 | 规则 | 示例 |
|---|---|---|
| 目标地址 | 必须解析为 `127.0.0.1` 或 `::1` | 拒绝 `0.0.0.0`、`localhost` 别名解析到外部 |
| 端口范围 | 1024-65535，排除 BLOCKED_PORTS | 拒绝 22、443、3306 等 |
| 协议 | 固定 `http://`，不接受 client 指定 | 拒绝 `https://`、`file://` |
| 路径 | 必须以 `/` 开头，不含 `://` | 拒绝 `@evil.com/`、`//evil.com/` |
| 重定向 | 不自动跟随 3xx 重定向 | 返回 3xx status 给 client 自行处理 |

---

## Notifications

Preview 域**没有 notification**。Preview Proxy 是无状态的请求/响应模式。

Client 可以自行实现轮询或通过其他机制（如 WebSocket 直连 dev server）获取 dev server 状态变化。

---

## Reconnect Resync 映射

Preview Proxy 是无状态服务，**没有 reconnect resync 需求**。

- Proxy 不维护任何跨请求状态——每次请求独立处理。
- Client 重连后可以直接发起新的 proxy 请求，无需恢复状态。
