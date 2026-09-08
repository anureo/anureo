# apps/acp 代码质量审查（2026-09-01）

状态：快照审查，仅针对"写得不好/不优雅"的代码质量坏味道；`runtime.rs` 被清空导致的编译失败不在范围内。
方法：15 个并行审查组逐行阅读 `apps/acp/src` 全部 94 个文件（约 5 万行），按重复代码 / 超长函数 / 不必要 clone / 吞错 / 死代码 / 魔法数字 / 错误处理一致性 / API 设计 / 命名 / 锁跨 await / 过度抽象 11 个维度扫描。
说明：组 9（pairing/relay/auth）与组 10（files/worktree/question）仅返回了汇总统计；其余 13 组为逐条明细。行号为审查时快照，后续改动会产生偏移。已人工抽查验证关键发现（-32011 出现 6 次、param_str 系列 8 处重复、base64 双实现、kill/release 重复均确认属实）。`lock().unwrap_or_else(|e| e.into_inner())` 为本仓 poison 恢复惯例，未计入吞错。

## 总览

原始发现约 240 条，去噪后约 200 条。数量分布上"不必要 clone"最多（~60），"吞错"次之（~40）；但从修复收益看，扩展层样板重复（T1）和吞错（T6）最值得先做。

## T1 扩展层参数解析/错误构造样板重复（影响 12+ 文件，建议优先修）

同一个 helper 在多个扩展文件里各复制一份：

- `param_str`：`skills.rs:9`、`plugin.rs:12`、`quota_provider.rs:18`、`terminal_ext.rs:362`
- `require_param(_str)`：`skills.rs:16`、`plugin.rs:19`、`quota_provider.rs:25`、`scheduled_task.rs:214`、`goal.rs:250`、`terminal_ext.rs:369`、`worktree.rs:190`、`git/mod.rs:304`
- `optional_param_str`：`git/mod.rs:315`、`worktree.rs:201`、`scheduled_task.rs:229`、`goal.rs:265`
- `internal()` 错误构造：`skills.rs:22`、`plugin.rs`、`snippet.rs`、`command.rs`、`multi_run.rs`、`session_folder.rs` 六处几乎相同
- `object_params` / `validate_text`：`snippet.rs:450/462` 与 `command.rs:525/545` 逐行重复
- `principal()`：`snippet.rs:158` 与 `command.rs:158` 完全相同
- git 子命令 `repo_dir` 获取：`stash.rs`（6 处）、`status.rs`、`diff.rs`、`branches.rs`、`remote.rs`、`staging.rs`、`merge_rebase.rs` 每个 handler 重复一遍
- mcp.rs id 解析：`mcp.rs:456-460` 与 `482-486` 重复
- 错误构造三连：`session_list.rs:538-560` 三个近似 error 构造块
- `is_write` 判断与 capabilities()/handle() 方法清单双份维护：`extensions/mod.rs:100/244`

建议：新建 `extensions/common.rs`（params + errors + repo_dir），git 组建 `git::util`。一次消除约 40 条重复。

## T2 手写 base64 双实现

`tts.rs:327`（`encode`）与 `preview.rs:220`（`base64_encode`）是几乎逐行相同的手写 base64。应改用 `base64` crate 或抽公共 util。

## T3 魔法数字 / 硬编码

- 错误码：`stdio_loop.rs` 的 `-32011`（332/385/485/540/624/643）与 `-32010`（447）；`git/mod.rs` 的 `-32005`。应定义错误码常量/枚举
- `agent.rs`：300（763，curator idle 默认值）、5（1592，checkpoint 重试）、200（1857，history 分页上限）、3/100（2427/2454，title 重试次数/延迟）、50（71）、4（1372，token 估算）
- `terminal.rs`：`CREATE_NO_WINDOW 0x0800_0000`（146）、3600（235）、3700（373）、4096（307）
- `session_repository.rs`：生命周期字符串 `'idle'`（295）/`'closed'`（1369）、busy_timeout 30000（278）、`9007199254740991`（395/401）
- 各扩展默认 limit 50/200/1000 散布：`mcp.rs:16-17`、`goal.rs:13-14`、`session_list.rs:33-34`、`scheduled_task.rs:13-15`、`small_model.rs:11-16`
- 其他：`settings.rs:741` reloadDelayMs 300、`config_entity.rs` snippet 15 次展开/80 字符、`project.rs:1077` 16KB 参数限制、`stream_bridge.rs` 8192/4096 上下文常量

## T4 超长函数（god functions）

- `agent.rs`：`prompt_with_capabilities` 300+ 行（1092）；`resolve_model_with_tier_awareness` 130 行（356）
- `stdio_loop.rs`：`run_agent_connection` 600+ 行（112-729），应拆请求分发/处理循环/错误处理
- `config_entity.rs`：`update_entity` 247 行且 9 参数（389）；`create_entity` 8 参数
- `files.rs`：`handle_search` 115 行；`handle_exec_commands` 107 行
- `multi_run.rs`：`create` 150 行（415）
- `stream_bridge.rs`：`stream_event_to_updates_inner` 140 行（268）；`send_history` 122 行（909）
- `notification_router.rs`：`route`/`flush_session`/`send_history_batch` 各 50-70 行且结构同构
- `diagnostics.rs`：`export` 156 行（565）
- `project.rs`：`handle` 100+ 行（712）；`apply_update` 90 行（594）
- `terminal.rs`：`spawn_exit_watcher`、`spawn_output_reader` 各 100+ 行
- `client_capabilities.rs`：`from_client_capabilities_json` 100+ 行（53）
- `session_repository.rs`：`delete_all_indexed` 116 行（1396）
- `tools/terminal_executor.rs`：两个 `execute` 均超长且 shell 包装逻辑重复（35/212）

## T5 不必要 clone / to_string（~60 处）

热点（按文件聚合）：
- `goal.rs`（375-551 约 8 处）、`scheduled_task.rs`（320-512 约 10 处）、`mcp.rs`（176-396 5 处）、`small_model.rs`（176-388 7 处）、`agent_profile.rs`（409-564 5 处）、`session_list.rs`（202-700 5 处）
- 热路径：`stream_bridge.rs` delta 处理 `content.clone()`（276/282/332）、状态字符串 `"running".to_string()`（303）
- `review_runner.rs:36` 连续 4 个 clone；`session_update_log.rs` 同一 `session_id.to_string()` 在 SQL 参数中反复构造（134/176/409）
- `session_repository.rs`：排序/循环内 `id.clone()`（152/240/241）、`"{}"`.into()` 每次分配（1070）

建议：编译恢复后以 clippy `redundant_clone` 类 lint 引导批量清理；热路径 stream_bridge 优先。

## T6 吞错（let _ = / .ok() / unwrap_or 无日志）

- `last_model.rs:18/20/24`：save/clear 静默失败，"上次选的模型"失效时无从排查
- `logging.rs:18/114/104`：create_dir_all、LOG_GUARD.set、config 加载静默
- `tools/fs_tools.rs:186`、`tools/client_bridge.rs:145-159`（cleanup 多处 let _）
- `quota_provider.rs:259/269/312`（cache lock `let Ok` 吞掉）、296（凭据 unwrap_or_default）
- `github.rs:452`（.ok() 吞 HTTP 错误）、`config_entity.rs:879/892`（read_dir/解析失败静默）
- `session_folder.rs:450`（publish 忽略）、`preview.rs:285`（unwrap_or(0)）、`settings.rs:683`（notify_others）
- git 组普遍：`branches.rs:56/98/143`、`diff.rs:45-98`、`worktree.rs:86-89`、`stash.rs:83`、`remote.rs:177` 的 `unwrap_or_default()` 把 git 命令错误吞成空值
- `stdio_loop.rs:62/145/158`、`cli_client.rs:96`、`metadata.rs:30`、`session_config_store.rs:14-19`（eprintln 而非 tracing）

## T7 死代码 / 占位残留

- `let _ = x;` 式占位：`terminal.rs:105`、`diagnostics.rs:392`、`notification.rs:502`、`tunnel.rs:502`、`git/worktree.rs:58/107`、`git/status.rs:30`、`git/merge_rebase.rs:201`（let _ = params）、`extensions/mod.rs:208`、`extensions/auth.rs:50`
- 未使用项：`github.rs:31`（user_code dead_code）、`github.rs:48`（active_token 整个未用）、`notification.rs:134`（bundle_id）
- `agent.rs:751` 附近注释掉的 Priority 说明

## T8 重复/同构逻辑（T1 之外的）

- `terminal.rs`：`kill`（418）与 `release`（458）终止逻辑重复，应互相复用
- 状态字符串 `"running"/"success"/"failure"` 硬编码散布 `stream_bridge.rs:303/376-391`，应枚举化
- `session_repository.rs`：`WITH RECURSIVE descendants` CTE 在 897/957/1228/1477 四处复制，且在循环内逐会话执行递归 SQL（610/896/955/1227/1475），应改为单次批量查询
- `agent.rs`：`config_store.set` 错误处理 ×3（875 起）；`current_model`/`current_effort` 获取逻辑在 925/1037/1731/1739 重复；affected_sessions JSON 构建与 781-802 重复（2034）
- `tools/client_bridge.rs:168-174` 每个 terminal 方法重复连接获取模式

## T9 API 设计

- 参数过多（应封装参数结构体）：`config_entity.rs` update_entity 9 参 / create_entity 8 参 / entity_sources 6 参；`session_repository.rs:513/543` insert_index_record(_once) 7 参；`agent.rs:2558/2631` build_*_response 6 参；`client_methods.rs:61` terminal_create；`stream_bridge.rs:1097` enable_high_freq_tracking_with_config 5 参
- bool flag 参数：`session_repository.rs:1181` set_archived_internal，建议枚举 ArchiveAction
- pub 过宽：`agent.rs:1823/1846` session_history_info/page 可收窄为 pub(crate)
- 错误处理风格混用：`session_history.rs:42/47` 用 expect 可能 panic；`project.rs:372` lock 错误转 to_string 与他处不一致；测试与主代码常量重复定义（session_repository 2368/2385）

## T10 锁与并发

- `stream_bridge.rs:666/1092/1120`：`high_freq_tracker.lock().unwrap()` 在 async 上下文持有跨 await
- `plugin.rs:451`：registry_cache 锁跨 await（网络请求前未释放）
- `multi_run.rs:423`：store.read() 可能跨 await
- `agent.rs:1360/2503`：usage acc 锁在 async 中持有较长
- `global_events.rs` 多处 `.lock().expect()`：同步上下文可接受，但 expect 会 panic，可统一为 poison 恢复惯例

## T11 命名 / 小项

- `stdio_loop.rs:168-194` `a_init`/`r_new` 等缩写；`stream_bridge.rs` 闭包参数 `n`；`project.rs:1026` `hash`；`github.rs:250` `author`；`agent.rs:194` `session_update_tx`
- `provider.rs:55` provider_id 兼容 providerID/providerId/id 三种字段名但未文档化
- `command.rs:515` Windows 盘根硬编码；`session_folder.rs:531` `items[items.len()-1]` 空数组越界风险

## 组 9 / 组 10 汇总（无逐条明细）

- 组 9（pairing 1823 行 / relay 1340 / connection 492 / client_auth 513 / session_auth 400 / auth 92）：30 条，P0×12。主题：超时/重试等参数硬编码、expect/unwrap/ok 混用、字符串校验与连接类型判断重复、锁跨 await 与嵌套锁
- 组 10（files 2233 / worktree 864 / question 845）：35 条，P0×9。主题：ReadTextResult 构造与相对路径计算重复、`handle_search` 115 行、吞错 7 处、pub 过宽

## 建议修复顺序

1. T1 抽 `extensions/common.rs`（一次消 ~40 条，风险低）
2. T6 吞错补日志（成本低、可观测性收益大）
3. T2 base64、T8 kill/release 等机械去重
4. T3 错误码与超时类魔法数字常量化
5. T9 参数结构体化 + T4 拆 god 函数（影响面大，配合测试做）
6. T5 clone 清理：先恢复编译，再由 clippy 引导批量处理
