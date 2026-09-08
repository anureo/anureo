# anureo 全仓代码质量审查清单（2026-09-01）

状态：全量快照审查。范围：workspace 全部 32 个 Rust crate（约 21.5 万行，不含 tests/、examples/）。
方法：55 个并行审查组逐行阅读，按 11 维度（重复代码/超长函数/深嵌套/不必要clone/吞错/死代码/魔法数字/错误处理不一致/API设计/命名/锁跨await）扫描。`lock().unwrap_or_else(|e| e.into_inner())` 为本仓 poison 恢复惯例，未计入。
配套：编码规范见 [docs/dev/coding-guide.md](../dev/coding-guide.md)；apps/acp 的逐条明细见 [acp-code-quality-review-20260901.md](acp-code-quality-review-20260901.md)。

## 一、总统计

- 原始发现约 **2000 条**（同一模式在同一文件逐行重复时按行计）；去重归并后 **13 大类、约 60 个主题**。
- 粗略类别分布（按原始条数）：不必要 clone/to_string ≈45%，吞错 ≈15%，重复逻辑 ≈15%，魔法数字 ≈10%，超长函数/深嵌套 ≈8%，API 设计 ≈4%，死代码 ≈2%，锁与并发 ≈1%。
- 明细级（有逐条行号）约 1500 条；汇总级（仅数量与主题）约 500 条（见第三节标注）。

## 二、全仓跨 crate 主题（按修复价值排序）

### X1 跨 crate 代码复制（必须优先）
- **uuid6 整文件双份**：`foundation/checkpoint/src/uuid6.rs` 与 `foundation/llm/src/support/uuid6.rs` 约 362 行完全重复（已确认）
- **手写 base64 双份**：`apps/acp/src/extensions/tts.rs:327` 与 `apps/acp/src/extensions/preview.rs:220` 逐行相同
- **llm provider 错误 parser 12 文件同构**：`foundation/llm/src/error/provider/*.rs` 的 `XxxParser` 结构体/new/kind_from_status/错误码映射模式重复 → 宏生成
- **vector-store 三实现重复**：`text_from_value`×3、`matches_condition`×2、`batch`×3、`ns_to_key`×2（in_memory/sqlite_vec/lance）→ trait 默认实现或公共模块
- **got/tot/dup 三套 runner**：`agent/agent-core/src/agent/{got,tot,dup}/` 的 SharedLlm、build 函数、adapter node 逐字相同 → 公共模块
- **file 工具参数解析样板**：`tool-basic/src/file/*` 的 `args.get("path").and_then(...).ok_or_else(...)`、`create_dir_all` 父目录模式 ×7 → `require_param_str`/`ensure_parent_exists`
- **扩展层参数样板**：apps/acp extensions 的 `param_str`/`require_param`/`internal()` 在 12+ 文件各一份
- **telegram 三个 send 工具**：API 获取+chat_id 解析完全重复（tool-extensions）
- **memory/task 工具同构**（tool-experimental）、**tool-workflow 五个 tool 同构**（tool_start/list/cancel/status/files + lib.rs 注册）→ 宏
- **channels 通道同构**（graph-core）、**MIME/扩展名映射**（telegram-bot download.rs 与 utils.rs 重复）、**skill pinned 检查×6、frontmatter 解析×5**（skill/storage.rs）
- **两套 openai client**（foundation/llm client/openai 与 client/openai_compat 并存，错误处理重试逻辑重复）

### X2 吞错（约 300 处）
重灾区（示例行号为代表）：curator_backup.rs（8 处 fs 操作 let _ =）、curator history.rs、checkpoint-sqlite-store sqlite_util.rs:147/156 与 repair.rs、vector-store 序列化 unwrap_or_default、lsp client.rs:361/550（shutdown_tx/process.kill）、codex agent.rs output_tx.send、pregel runner.rs tx.send、tool-basic file 工具、agent-core tools unwrap()×5、task-cli/task-mcp-server unwrap()/expect()、telegram-bot handler_deps/session、apps/server logging.rs、acp last_model/logging（详见各报告）。
模式三种：`let _ =`（IO/send）、`.ok()`/`unwrap_or_default()`（解析/锁）、`.unwrap()`/`.expect()`（可能失败路径）。规范见 coding-guide §2。

### X3 超长函数（>100 行）
最严重（行数为审查快照）：stdio_loop.rs run_agent_connection 600+；codex agent.rs handle_turn_start 216-440；curator.rs 整文件 2739 行；agent.rs prompt_with_capabilities 300+；config_entity.rs update_entity 247；multi_run.rs create 150；models_dev resolver.rs get 130+；model_registry.rs list_all_models_inner 129；git2_backend.rs collect_diff_summary_pub 127/commit_blocking 113；pregel runtime invoke_inner 133/replay 101/apply_writes 108、loop_state tick/after_tick、channel push×2、runner run_step/run_task；checkpoint-sqlite-store get_tuple/search/list_namespaces/batch/list 5 个 100+；sqlite_vec lance search 108；subcommands.rs Curator 分支 180+；skill storage 144；diagnostics export 156；files handle_search 115。

### X4 魔法数字/硬编码
错误码 -32010/-32011（acp stdio_loop ×6）、-32005（git）、-32001（server acp.rs）；安全常量（auth.rs：12h/7d TTL、scrypt Params(14,8,1,64)、限速、pre-auth 30s）；超时/重试（lsp error_recovery 8 个默认值、client 30s/5s/3次/200ms）；成本阈值 0.5/15.0（model-spec tier.rs）；状态字符串 'idle'/'closed'/"running"/"success"/"failure"；UUID 常量 0x01b2_1dd2_1381_4000、0xDEAD_BEEF_CAFE_BABE；limit 50/200/1000/10000 散布各扩展；MAX_FILE 5MB、preview 1200 等散布 cli。

### X5 锁与并发
- async 中持 std Mutex 跨 await：acp stream_bridge high_freq_tracker（3 处）、plugin registry_cache、multi_run store.read()、agent usage acc
- `lock().unwrap()` / `.lock().ok()` 混用（approval.rs、cache.rs、node.rs 等）；应统一 poison 惯例
- `Arc<Mutex<bool>>`（git2_ops:557）→ AtomicBool；SeqCst 滥用（telegram-bot health.rs）→ Relaxed
- 嵌套锁：acp pairing/relay 组报告多把锁嵌套

### X6 死代码/占位
github.rs active_token/user_code、lsp mock_server 5 处 allow(dead_code)、installer package_managers、cli skill_inspect BuiltinSkillContribution、tool-basic batch working_folder、google/openrouter error code 字段、多处 `let _ = ctx/params/path` 占位。

### X7 API 设计
- 参数 >5：config_entity create/update（8/9）、got runner new（11）、tot runner new（12）、dup new（9）、insert_index_record 7、react_build_config new、lsp manager 4 个方法 5 参、register_file_tools
- bool flag：session_repository set_archived_internal
- pub 过宽：agent.rs history API、lsp/workspace 等

## 三、分 crate 明细统计

| Crate（组） | 发现数 | 明细级 | 最严重问题 |
|---|---|---|---|
| apps/acp（15 组） | ~235 | 13 组明细 | 扩展样板复制、-32011×6、god functions、锁跨 await |
| apps/cli（4 组） | C1 33 / C2 86 / C3 49 / C4 55 | C1/C3 明细，C2/C4 汇总 | session.rs inner.clone()×11、Curator 分支 180+ 行、HTTP body 重复 clone |
| agent-core（4 组） | A1 32 / A2 41 / A3 58 / A4 38 | 全部明细 | got/tot/dup 三套重复、react id 解析重复、unwrap()×5 |
| foundation/llm（2 组） | L1 42 / L2 41 | 全部明细 | 两套 openai client、provider parser 12 文件同构、uuid6 重复（与 checkpoint） |
| tool-basic（3 组） | TB1 83 / TB2 38 / TB3 40 | 全部明细 | file 工具参数样板 ×7、skill manage 889 行、bash/powershell 同构 |
| foundation/git（2 组） | G1 30 / G2 38 | 全部明细 | 3 个 110+ 行函数、hunk_cb 深嵌套、Arc<Mutex<bool>> |
| experimental/curator（3 组） | CU1 58 / CU2 45 / CU3 22 | 全部明细 | curator.rs 2739 行 god file、curator_backup 8 处吞错、SkillRegistry 重复创建 |
| apps/telegram-bot（2 组） | TG1 44 / TG2 27 | 全部明细 | MIME 映射重复、compact/summarize 重复、SeqCst 滥用 |
| tool-workflow（1 组） | 95 | 明细 | 5 个 tool 同构样板、95 条中 58 条 clone/to_string |
| foundation/pregel（2 组） | PG1 86 / PG2 47 | PG2 明细，PG1 汇总 | 6 个 100+ 行函数、吞错 tx.send、cache 双重 clone |
| agent/skill（2 组） | SK1 56 / SK2 42 | 全部汇总 | pinned 检查×6、frontmatter 解析×5、144 行函数 |
| experimental/tool/lsp（1 组） | 38 | 明细 | error_recovery 8 个魔法默认值、mock_server 5 处 dead_code、吞错 process.kill |
| model-spec-core（1 组） | 32 | 明细 | list_all_models_inner 129 行、parse_* 4 函数与 parser.rs 完全重复 |
| foundation/config（1 组） | 32 | 明细 | log_format/xdg_toml 长函数、dotenv 静默 |
| graph-core（1 组） | 36 | 汇总 | channels/ 6 个通道同构（LastValue/EphemeralValue 几乎逐字重复） |
| stream-event（2 组） | SEV ~30 / SEV2 32 | 汇总 | stream_writer emit 模式 ×14 重复、clone 热点 |
| checkpoint（1 组） | 36 | 汇总 | uuid6 362 行与 llm 重复（已确认）、clone 密集 |
| checkpoint-sqlite-store（1 组） | 43 | 明细 | 5 个 100+ 行函数、repair/sqlite_util 吞错、ALTER TABLE 样板重复 |
| apps/server（1 组） | 27 | 明细 | auth.rs 安全常量全硬编码（P0）、环境变量解析重复 |
| tool-experimental（1 组） | 41 | 汇总 | memory/task 工具同构样板 |
| vector-store（1 组） | 39 | 明细 | 三 store 重复函数、序列化吞错、limit 1000×3 |
| memory-v2 + worktree（1 组） | 34 | 明细 | to_string_lossy().to_string() 模式、循环内 to_lowercase |
| task-core + codex + task-cli/mcp（1 组） | 42 | 明细 | handle_turn_start 224 行、unwrap/expect 入口、SQL 构建重复 |
| tool-core + tool-extensions（1 组） | 22 | 明细 | telegram send 三胞胎重复、registry clone |
| anureo-util + pty-protocol 等（1 组） | 26 | 明细 | fuzzy_replace 循环内 Regex::new（性能 P0）、相似度重复计算死代码 |

汇总级组（仅主题无数值明细）：C2、C4、PG1、SK1、SK2、GC、CKPT、TEXP、SEV2 及 acp 组 9/10，共约 535 条。

## 四、修复路线建议

1. **机械收益最大**：X1 跨 crate 复制合并（uuid6/base64/provider parser/vector-store/file 样板）≈ 消除 200+ 条
2. **可观测性**：X2 吞错补日志（约 300 处，模式固定可批量）
3. **规范固化**：X4 魔法数字常量化、状态 enum 化
4. **结构**：X3 god functions 拆分（配合测试）；X5 锁规范统一
5. **长期**：X7 API 参数结构体化；同构实现族宏化
