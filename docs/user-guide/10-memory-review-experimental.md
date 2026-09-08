# Memory、Review、Skill Usage 与 Curator（实验性）

> **实验性功能警告**：本文涉及 `memory-v2`、`curator`，以及 `review`、`review-skill`、`skill-usage` 和 skill lifecycle 路径。它们的文件格式、默认路径、命令参数和副作用都可能变化；使用前请以当前 checkout 的 `anureo --help`、对应子命令的 `--help` 和 source 为准。它们不是默认开启的自动 review/curation，也不提供生产知识库、合规归档或跨用户同步保证。

本文面向希望跨 session 保存项目事实/偏好、从 session 提取 skills，或试用 skill lifecycle 管理的高级开发者。模型生成的 memory/skill update 只是候选变更，必须像代码一样审查；不要把它们直接当作事实。本文不介绍未实现的 `evolve`，也不展开 memory/vector-store 的内部算法。

## 先确认边界与备份

anureo 的默认配置目录是 `~/.anureo/`（可由 `ANUREO_HOME` 覆盖），但 curator backup 使用平台的 local-data directory，独立于 `ANUREO_HOME`；Windows 正常环境通常类似 `%LOCALAPPDATA%\anureo\backups`。请以 `anureo curator backup` 的输出发现实际 backup directory。当前实现使用的主要对象包括：

- memory：`~/.anureo/data/memory/USER.md` 与 `PROJECT.md`；
- review history：`~/.anureo/memory.db`，其中有 append-only 的 `review_history` 和每个 session 的最新 `review_status`；旧版 `~/.anureo/data/review/history.jsonl` 可能在首次打开时迁移为 `.jsonl.bak`；
- skills：默认由 `skill_registry::default_path()` 解析，通常是 `~/.anureo/data/skills`；skill 内容、frontmatter/lifecycle、`.usage.json`、`.curator_suppressed` 和 `.archive/` 都在该 skill tree 周围；
- curator snapshots：由 `CuratorBackup` 管理的 `curator-*.tar.gz` 和 rollback 的 `pre-*.tar.gz`。具体 backup directory 由当前实现决定，请用 `anureo curator backup` 的输出确认；`anureo curator snapshots` 只列出名称以 `curator-` 开头的 snapshots，`pre-*` 和临时 rollback artifacts 需直接检查 backup directory。

本 checkout 的相关 source 入口是 `apps/cli/src/run/memory.rs`（re-export `memory-v2`）和 `apps/cli/src/run/memory_provider.rs`；curator CLI 主要在 `apps/cli/src/subcommands.rs`，实现位于 `experimental/memory-v2/src/` 与 `experimental/curator/src/`。这些路径是当前 checkout 的参考，不是对其他分支的承诺。

在编辑、审查、归档或 rollback 前，先确认具体 target，并保存可恢复副本：

```powershell
anureo --help
anureo memory --help
anureo review --help
anureo review-skill --help
anureo skill-usage --help
anureo curator --help
anureo memory show
anureo curator status
anureo curator snapshots
```

至少检查 `ANUREO_HOME`、skill root、session ID 和当前 Git/文件状态；不要把 secrets 放进 prompt、memory、skills 或 logs。编辑 memory 前复制 `USER.md`/`PROJECT.md`；修改 `.usage.json` 前复制它；对 skill lifecycle 操作，优先先执行 `anureo curator backup --description "before <operation>"`，并记录 snapshot filename。rollback 前还要核对 snapshot manifest、当前 skill root 和目标 snapshot；`--capture-pre` 默认开启，但仍应把它当作恢复保险而不是替代人工备份。

## Memory：两个会进入未来 context 的文件

`memory-v2` 将 memory 分成两个对象：`USER.md`（用户是谁、偏好）和 `PROJECT.md`（项目事实、agent notes、持久知识）。每个文件有独立容量：`USER.md` 约 4000 chars，`PROJECT.md` 约 8000 chars；system prompt 的 memory section 还受默认 `max_memory_chars = 8000` 限制。foreground 写入带有 `assistant_tool`/`foreground` provenance；review 写入带有 `background_review` provenance 和 session 关联。

若 agent 配置允许，`PROJECT.md` 和 `USER.md` 会分别作为 system-prompt context 注入后续 Agent。关闭 `memory_enabled` 会跳过 project memory，关闭 `user_profile_enabled` 会跳过 user memory。因此，修改任一文件都可能改变未来 session 的初始 context；删除或改写事实同样会改变行为。

### 查看、编辑、搜索

```powershell
anureo memory show
anureo --json memory show
anureo memory search "cargo test"
anureo --json memory search "Japanese"
anureo memory edit PROJECT
anureo memory edit USER.md
```

`show` 读取两个文件；`search` 在两文件逐行做 case-insensitive substring search，输出文件、行号和匹配行。`edit` 只接受 `USER`/`USER.md` 或 `PROJECT`/`PROJECT.md`/`MEMORY`/`MEMORY.md`，用 `$EDITOR`（未设置时为 `vi`）打开临时文件。编辑器成功退出后，当前实现把整份编辑结果交给 `add_entry`：它会去重、检查容量并追加为一个 entry，而不是提供逐条 replace/delete 编辑器。因此不要在未备份的情况下把 `edit` 当作安全的原地编辑；编辑器取消或失败不会写回，临时文件随后会被删除。memory 命令没有 dry-run。

`memory-v2` 的 entry 以 `§` 分隔。迁移取决于操作：`capture_snapshot`、`read_entries`、`add` 及相关 entry API 读取旧的空行格式时可以迁移并原子写回；`capture_snapshot` 还可以把旧 `FACTS.md` 迁移到 `PROJECT.md`。当前 CLI 的 `memory show` 使用直接 `load`，`memory search` 也通过该 raw-read 路径读取，因此不要把仅仅查看这些 CLI 视图当作迁移操作。这类会触发迁移的操作可能产生文件变更，所以在处理旧数据前先备份 memory directory。

## Review：从 session 生成候选 memory/skill 更新

review 使用 session 文本、受限的 review tools 和选定 model；它可以写 memory、skill，也会把结果计数和状态写入 `~/.anureo/memory.db`。审查输出中的 action、summary、`memory_count`、`skill_count` 只是执行报告，不是事实证明。

### 单个或批量 session

```powershell
anureo review pending --limit 20
anureo review --model <MODEL> session <SESSION_ID> --trigger manual
anureo review --model <MODEL> sessions --recent 7d
anureo review --dry-run sessions --all-unreviewed
anureo review --memory-only sessions --query "database"
```

- `review session <SESSION_ID>` 从 session manager 提取文本。少于 200 chars 的 session 不调用 LLM，会标记为 skipped 并写 review record；正常运行可能写 memory/skills，并追加 history。
- `review sessions` 必须指定且只按当前实现选择 `--recent Nd`、`--all-unreviewed` 或 `--query <text>` 之一。query 最多取 100 个匹配 session；`--all-unreviewed` 依据 review status 排除已处理（包括 skipped）的 session。
- `--dry-run` 只列出将处理的 session；单 session 会读取并报告文本长度，batch 会列出 ID/标题/时间，不调用 LLM、不写 memory/skills，也不追加 review history。
- `--memory-only` 关闭 skill review；`--skills-only` 关闭 memory review。两者不要同时使用：当前实现会同时关闭另一侧，结果基本没有可写对象。
- `--model` 覆盖 review model；未指定时按当前 config/env/provider fallback 选择。`--verbose` 只影响显示内容，`--json` 是全局输出选项。

```powershell
anureo review history --limit 20
anureo review history --trigger manual --json
anureo review show <SESSION_ID>
anureo review pending --limit 50
```

`history` 查看最新审查记录（可按 trigger 过滤）；注意当前实现先取最新 N 条总体记录，再应用 trigger filter，所以匹配记录可能少于 N。`show` 查看某 session 的最新 status，尚未处理则显示 pending；`pending` 列出尚未出现在 review status 中的 sessions。`history` 是 append-only，但同一 session 的 `review_status` 会被最新记录 upsert；因此重审会保留历史，同时改变 pending 判定。打开 history 可能初始化 `memory.db` schema，旧 JSONL 迁移成功后重命名为 `.jsonl.bak`。

### `review-skill`：stdin 或文件

```powershell
Get-Content .\session-extract.md | anureo review-skill --model <MODEL>
anureo review-skill --input .\session-extract.md --model <MODEL>
```

不提供 `--input` 时从 stdin 读到 EOF；提供时读取该 file。空输入直接失败。`--model` 覆盖当前 config/env/provider 选择（默认 fallback 由 source 决定）。该命令使用新的 UUID checkpoint 和默认的 memory+skill review config，没有 `--dry-run`、`--memory-only` 或 `--skills-only`；成功时可能立即写入 memory/skills。因此先把输入文件固定下来并备份目标目录，review 结果仍需人工逐项审查。

## Skill usage：使用统计与 `.usage.json`

`.usage.json` 是 skill root 下按 skill name 记录的 usage object，字段包括 use/view/patch counts、时间戳、state 和 pin 等状态。它会影响 curator 的 stale/archival 判断；不要手工把统计数字当作事实。

```powershell
anureo skill-usage sync --dry-run
anureo skill-usage sync --path .\path\to\skills --dry-run --json
anureo skill-usage sync
anureo skill-usage show
anureo skill-usage show <SKILL_NAME> --json
anureo skill-usage repair --path .\path\to\skills
```

`sync` 扫描指定或默认 skill root：现有 entry 保留，缺少的 skill 创建新的 `SkillUsage` entry；`--dry-run` 不写文件。当前 CLI handler 尚未使用 `--source` 的值进行过滤，虽然 help 声明了 `auto`、`curated`、`evolved`、`all`，不要假设它会缩小扫描范围。`show` 只读取默认 root（当前命令没有 `--path`），可显示全部或一个 skill。`repair` 检查 `.usage.json`；JSON 解析损坏分支会先复制为 `.usage.bak`，尝试恢复有效 entries，然后重写文件。空文件不会创建该 backup；不可读内容可能先得到恢复结果，但当前 handler 随后仍可能把文件重初始化为空 object，因此不要假定恢复 entries 已保留。repair 没有 dry-run，会改变 `.usage.json`，所以先复制并检查具体 path。

## Curator：skill lifecycle 的实验性管理

Curator 处理 skill metadata/lifecycle、usage 和 archive tree。`run` 会跳过 pinned skills，按 source 和 idle days 将 Active 标为 Stale，再把长期 stale skill 标为 Archived 并移动到 `.archive/`；也会报告 active skill 的相似/overlap。非 dry-run 的 `run` 在 mutation 前尝试创建 pre-run snapshot，并在失败时继续，因此仍需检查实际 snapshot 是否存在。

```powershell
anureo curator status
anureo curator --dry-run run
anureo curator run --force
anureo curator run --no-consolidate
anureo curator run --background
anureo curator --watch 3600 run
anureo curator --dry-run prune --days 90
anureo curator prune --days 90 --yes
```

`run` 的 LLM consolidation 当前默认运行；`--consolidate` 是兼容性参数，`--no-consolidate` 才跳过。当前 one-shot CLI path 接受 `--force` 但不使用它，也不调用 curator 的 interval-gating 判断，因此不要把它当作绕过 gating 的开关；实际是否运行仍以当前 CLI/source 为准。`--background` 当前也只是接受该参数，Phase 1 future 仍被同步 await，命令不会 fire-and-forget。`--watch SECONDS` 进入 recurring loop。`--dry-run` 只报告，不改 lifecycle/state，也不做 pre-run snapshot。`prune` 把指定 days 同时作为 stale/archive 阈值；非 dry-run 会提示确认，除非 `--yes`。不要在不了解全 tree 的情况下使用 `--yes` 或 `--background`。

```powershell
anureo curator pause
anureo curator resume
anureo curator pin <SKILL_NAME>
anureo curator unpin <SKILL_NAME>
anureo curator archive <SKILL_NAME>
anureo curator list-archived
anureo curator restore <SKILL_NAME>
```

`pause`/`resume` 修改 curator state，影响后续 scheduled runs；它们不是一次性取消正在运行的进程。`pin` 使 curator 不 archive 或 consolidate 该 skill，`unpin` 恢复管理；当前实现只允许 agent-created/curated skills，bundled/hub-installed skills 被视为 read-only。当前 CLI 的 `archive` 和 `restore` handler 只设置 lifecycle enum：`archive` 把 skill 标为 `Archived`，`restore` 把它标为 `Active`；它们不会调用 curator 的 physical archive/restore 方法，因此不要据此假定目录已移动、suppressed name 已写入或冲突已用 timestamped name 处理。命令本身也没有独立确认提示。若当前版本增加了 physical lifecycle path，请以该版本 source 和错误/确认输出为准。

## Backup、snapshots 与 rollback

```powershell
anureo curator backup --description "before manual archive"
anureo curator snapshots
anureo curator rollback curator-<timestamp>.tar.gz
anureo curator rollback --yes --capture-pre true curator-<timestamp>.tar.gz
```

`backup` 打包 skill tree，但会排除实现定义的 bookkeeping directories，至少包括顶层 `.hub` 和 `.curator_backups`；如需完整 forensic backup，请直接检查 archive 内容和 exclusions。命令返回 snapshot filename；`snapshots` 列出符合 `curator-*` 的 snapshot 的数量、大小、description 和时间，不包含 `pre-*` 或临时 rollback artifacts。`rollback` 不带 filename 时当前实现选择最新 `curator-*` snapshot；它会先显示 manifest summary，再要求 `y/N` 确认，`--yes` 跳过确认。默认 `--capture-pre` 为 true，rollback 前会把当前 active library 保存为编号的 `pre-<timestamp>-<n>.tar.gz`，之后才应用目标 snapshot。

rollback 会用 archive 内容替换当前 skill tree，可能撤销自 snapshot 以来的新增、编辑、archive 状态和 metadata；snapshot 不包含 session 或 memory，不能恢复 `USER.md`、`PROJECT.md`、`memory.db` 或 `.usage.json` 的独立变更。检查具体 snapshot、manifest、当前目标目录和 pre-rollback 文件；确认 archive 可读、目标未写错，再执行。若 rollback 失败或结果异常，停止后先列出 snapshots 和目录状态，不要连续覆盖式尝试。

## 哪些变更会改变未来 Agent context

会直接或间接影响后续 context 的包括：

1. 写入、删除、重排或超出容量失败前的修改 `USER.md`/`PROJECT.md`；
2. review 或 review-skill 通过 memory/skill tools 接受的 action；
3. skill 的内容、frontmatter、lifecycle、pinned 状态、`.archive/` 位置和 `.usage.json` 统计；
4. curator 的 state（pause、last run、last-used）、suppressed names，以及 archive/restore；
5. provider/model 造成的 review 输出差异，以及 review history 对 `pending`/`all-unreviewed` 的判定。

相反，`memory show/search`、`review ... --dry-run`、`curator run/prune --dry-run` 和 `curator snapshots` 主要是查看或预览；但读取旧 memory 可能触发格式迁移，且 `review history` 首次打开可能初始化/迁移数据库，仍应把结果纳入备份与 diff 检查。实验性实现会变化：每次操作都以当前 source 和 `--help` 为准。
