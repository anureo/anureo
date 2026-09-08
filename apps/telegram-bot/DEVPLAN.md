# Telegram Bot — Development Plan

**基于**: `REVIEW.md` (2025-08-19, 评分 6.5/10)
**目标**: 将代码质量提升至 8.5+ 分，消除所有已知 P0/P1 问题
**周期**: 4 个 Sprint (每 Sprint 约 1 周)

---

## Sprint 1 — 基础安全 (P0)

> 目标：消除 panic 风险、安全漏洞、编译阻塞

### Task 1.1 修复上游编译错误

**问题**: `anureo` crate 的 `ToolCallContent` 缺少 `len()` 方法，阻塞全部编译和测试
**位置**: `anureo/src/llm/openai/request.rs:123`, `anureo/src/user_message/sqlite_store.rs:175`
**工作量**: S (0.5d)
**验收**:
- [ ] `cargo build -p telegram-bot` 成功
- [ ] `cargo test -p telegram-bot` 可运行

### Task 1.2 retry.rs — 消除 unwrap + 指数退避

**问题**: 8 个 `unwrap()` 在重试关键路径，固定 1s 重试间隔
**位置**: `src/streaming/retry.rs` (137 行)
**工作量**: M (1.5d)

**具体改动**:

```
retry.rs 重构方案:
├── 引入 RetryPolicy enum: Transient / RateLimited / Fatal
├── classify_error() — 按 teloxide RequestError 分类
├── backoff_duration() — exponential backoff + jitter
│   base = 1s, factor = 2, jitter = ±25%, max = 30s
├── 重试循环: match error_type
│   Transient   → backoff + retry
│   RateLimited → parse Retry-After header, sleep + retry
│   Fatal       → return immediately
└── 所有 last_error.unwrap() → unwrap_or_else(fallback)
```

**验收**:
- [ ] 0 个 `unwrap()` 在 retry.rs 生产代码
- [ ] 单元测试覆盖: Transient 重试成功、RateLimited 等待、Fatal 立即返回
- [ ] 日志输出包含退避时间

### Task 1.3 download.rs — 路径遍历防护

**问题**: 用户文件名未验证，可能 `../../etc/passwd`
**位置**: `src/download.rs:76-84` (`get_file_path`)
**工作量**: S (0.5d)

**具体改动**:

```rust
// get_file_path 中添加:
fn sanitize_filename(name: &str) -> String {
    Path::new(name)
        .file_name()                    // 剥离目录前缀
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .replace(|c: char| !c.is_alphanumeric() && c != '.' && c != '-' && c != '_', "_")
}

// get_file_path 返回前验证:
fn validate_path(path: &Path, base_dir: &Path) -> Result<PathBuf, BotError> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let base = base_dir.canonicalize().unwrap_or_else(|_| base_dir.to_path_buf());
    if !canonical.starts_with(&base) {
        return Err(BotError::Security("path traversal detected"));
    }
    Ok(canonical)
}
```

**验收**:
- [ ] 正常文件名工作不变
- [ ] `../../etc/passwd` → `Security` error
- [ ] 空/特殊字符文件名 → fallback to `unknown`
- [ ] 单元测试覆盖 5+ 种异常输入

---

## Sprint 2 — 错误处理 & 并发 (P1)

> 目标：系统性消除 unwrap，加强并发安全

### Task 2.1 全局 unwrap 清理

**问题**: 生产代码中 46 个 `unwrap()` (retry.rs 8 个已在 Sprint 1 处理)
**位置**:
| 文件 | 数量 | 处理方式 |
|------|------|----------|
| `event_mapper.rs` | 5 | `unwrap_or_default` + `expect` |
| `config/telegram.rs` | 1 | `?` propagation |
| `mock.rs` | 19 | `expect("意图描述")` |
| `tests/*` | 13 | 保持不变 (测试代码) |

**工作量**: M (1d)
**验收**:
- [ ] `src/` 非 test 代码中 `unwrap()` ≤ 0 (不含 mock.rs)
- [ ] mock.rs 全部 `unwrap()` → `expect("...")`

### Task 2.2 event_mapper 锁优化

**问题**: RwLock 持有期间 clone，热路径性能瓶颈
**位置**: `src/streaming/event_mapper.rs:140,156,195`
**工作量**: S (0.5d)

```rust
// Before: 持锁 + clone
let phase = self.phase_state.read().unwrap().0.clone();

// After: 缩小锁范围
let phase = {
    let guard = self.phase_state.read().expect("phase_state lock poisoned");
    guard.0.clone()
}; // guard dropped here
```

**验收**:
- [ ] 锁持有范围最小化
- [ ] 无逻辑变更，现有测试通过

### Task 2.3 添加并发安全测试

**工作量**: M (1d)

```
新增测试:
├── tests/concurrency_test.rs
│   ├── test_concurrent_message_dispatch — 多消息同时到达
│   ├── test_cancel_during_agent_run — CancellationToken 触发
│   └── test_streaming_handler_drop_safety — handler drop 时 channel 关闭
├── tests/error_path_test.rs
│   ├── test_network_timeout — 模拟网络超时
│   ├── test_rate_limit_429 — 模拟 Telegram 429
│   └── test_agent_failure_recovery — Agent 返回错误后恢复
```

**验收**:
- [ ] `cargo test` 含并发测试，无死锁
- [ ] 错误路径测试覆盖 ≥ 5 个场景

---

## Sprint 3 — 性能 & 常量治理 (P1/P2)

> 目标：消除魔术数字、优化热路径

### Task 3.1 魔术数字提取

**工作量**: S (0.5d)

```rust
// src/constants.rs (新建)
pub(crate) mod streaming {
    pub const EDIT_THROTTLE_MS: u64 = 300;    // Telegram 建议最小编辑间隔
    pub const MAX_MESSAGE_LEN: usize = 4096;   // Telegram 单消息上限
    pub const THINK_HEADER: &str = "💭 Thinking...";
}

pub(crate) mod retry {
    pub const MAX_RETRIES: u32 = 3;            // 平衡用户体验与 API 压力
    pub const BASE_DELAY_SECS: u64 = 1;        // 初始退避基数
    pub const MAX_DELAY_SECS: u64 = 30;        // 退避上限
    pub const BACKOFF_FACTOR: u32 = 2;         // 指数退避乘数
}

pub(crate) mod model {
    pub const SEARCH_PAGE_SIZE: usize = 8;     // 移动端友好的列表长度
}

pub(crate) mod download {
    pub const MAX_FILENAME_LEN: usize = 24;    // file_id 截断长度
    pub const MAX_EXT_LEN: usize = 10;         // 扩展名最大长度
}
```

**验收**:
- [ ] 所有魔术数字提取到 `constants.rs`
- [ ] 每个常量有注释说明取值依据

### Task 3.2 克隆审计 & 优化

**工作量**: M (1d)

**优先处理的热路径**:

| 位置 | 当前 | 优化 |
|------|------|------|
| `event_mapper.rs:195` | 持锁 clone phase | 缩小锁范围后 clone |
| `message_handler.rs` | 每次 edit 都 clone FormattedMessage | 共享 Arc<str> |
| `sender.rs` | 多次 clone String | 改为 &str / Cow |

**验收**:
- [ ] 热路径 clone 减少 ≥ 30%
- [ ] 无功能回归

### Task 3.3 动态节流

**问题**: 固定 300ms 编辑间隔
**位置**: `src/streaming/message_handler.rs`
**工作量**: M (1d)

```rust
// 自适应节流策略:
fn adaptive_throttle(last_edit_len: usize) -> Duration {
    let base_ms: u64 = 300;
    if last_edit_len > 3000 {
        Duration::from_millis(base_ms * 2)     // 大消息降频，减少 Telegram API 压力
    } else if last_edit_len < 200 {
        Duration::from_millis(base_ms / 2)     // 小消息提频，更流畅
    } else {
        Duration::from_millis(base_ms)
    }
}
```

**验收**:
- [ ] 节流间隔根据消息长度自适应
- [ ] 单元测试验证各长度区间的间隔值

---

## Sprint 4 — 文档 & 工程化 (P2)

> 目标：完善文档、添加 benchmark、提升可维护性

### Task 4.1 rustdoc 补全

**工作量**: M (1d)

**范围**: `lib.rs`, `traits.rs`, 所有 `pub` 函数/struct/enum
**验收**:
- [ ] `cargo doc -p telegram-bot --no-deps` 无 warning
- [ ] 所有 `pub` item 有 `///` 或 `//!` 文档

### Task 4.2 性能基准测试

**工作量**: S (0.5d)

```
benches/
└── streaming_bench.rs
    ├── bench_event_mapper_throughput — 事件映射吞吐量
    ├── bench_message_handler_throttle — 节流器性能
    └── bench_format_large_message — 大消息格式化耗时
```

**依赖**: `criterion` (dev-dependency)
**验收**:
- [ ] `cargo bench -p telegram-bot` 可运行
- [ ] 3+ 基准测试覆盖核心热路径

### Task 4.3 删除冗余模块

**工作量**: XS (0.25d)

- [ ] 删除 `handler.rs` (仅 re-export `router::default_handler`)
- [ ] 将引用更新为直接 use `router::default_handler`

### Task 4.4 CI 集成

**工作量**: S (0.5d)

```yaml
# .github/workflows/telegram-bot.yml
on: [push, pull_request]
jobs:
  check:
    steps:
      - cargo clippy -p telegram-bot -- -D warnings
      - cargo test -p telegram-bot
      - cargo doc -p telegram-bot --no-deps
```

**验收**:
- [ ] PR 自动触发 clippy + test + doc check
- [ ] clippy warning 阻断合并

---

## 时间线总结

```
Week 1 (Sprint 1): Task 1.1 + 1.2 + 1.3   — 编译修复 + retry 重构 + 路径安全
Week 2 (Sprint 2): Task 2.1 + 2.2 + 2.3   — unwrap 清理 + 锁优化 + 并发测试
Week 3 (Sprint 3): Task 3.1 + 3.2 + 3.3   — 常量治理 + clone 优化 + 动态节流
Week 4 (Sprint 4): Task 4.1 + 4.2 + 4.3 + 4.4 — 文档 + bench + 清理 + CI
```

## 依赖关系

```
Task 1.1 (编译修复) ──→ 所有后续 Task
Task 1.2 (retry 重构) ──→ Task 2.3 (并发测试)
Task 1.3 (路径安全) ──→ 无依赖
Task 3.1 (常量提取) ──→ Task 3.3 (动态节流)
```

## 预期成果

| 指标 | 当前 | Sprint 4 后 |
|------|------|-------------|
| `unwrap()` (生产代码) | 27 | 0 |
| 编译 | ❌ 失败 | ✅ 通过 |
| clippy warnings | 未知 | 0 |
| rustdoc 覆盖 | ~30% | 100% (pub API) |
| 并发测试 | 0 | 3+ |
| 错误路径测试 | 少量 | 5+ |
| 魔术数字 | ~8 | 0 |
| 性能基准 | 无 | 3+ |
| 总评分 | 6.5/10 | 8.5/10 |
