# Skills 与 agent profiles

> **状态**：已实现；本文只描述当前源码暴露的稳定发现、检查、创建、编辑、删除与导出流程。`skill-usage`、`curator` 和 `evolve` 属于实验性维护能力，不是基础 skill 教程的一部分。

Skills 用来沉淀可复用的项目规范、提示、操作知识和参考资料；agent profile 用来沉淀一类任务的角色、模型、工具和 skill 组合。它们都会进入后续任务的上下文，所以在接受或修改可复用内容前，先确认 scope 和 source。

## 前提与发现范围

从项目根目录执行命令，并先确认 `--working-folder` 指向正确的项目边界。需要使用 `--agent` 时，profile 名称来自项目 `.anureo/agents/<NAME>` 或用户 `~/.anureo/agents/<NAME>`；内置 profile 也可能由 anureo 提供。

Skill discovery 使用以下顺序：

1. 项目目录 `<working-folder>/.anureo/skills`，source 为 `Project`。
2. profile 配置中的额外 skill directories，source 为 `ProfileDir`，CLI 过滤器显示为 `Profile`。
3. 用户目录 `~/.anureo/skills`（`ANUREO_HOME/skills`），source 为 `User`。
4. `~/.anureo/data/skills`（`ANUREO_HOME/data/skills`），递归扫描，source 为 `Data`。

发现结果按 skill name 去重，先发现的 source 优先。agent profile 运行时还会把该 profile 的 `<profile source>/skills` 加入 registry，source 为 `Agent`；Builtin skill 由 anureo crate 注入，并且同名的 filesystem skill 优先于 Builtin。`anureo skills inspect` 为了展示原始发现结果会保留这些 source，并注入当前 CLI 已知的 Builtin skill。

Skill 通常是一个目录中的 `SKILL.md`。目录可以包含 `references/`、`templates/`、`scripts/` 和 `assets/` 等 supporting files；加载 skill 时，目录中的其它文件会作为 additional resources 暴露给 agent，`references` 可由 agent 按需读取。frontmatter 中的 `description`、`triggers`、tags、conditions、prerequisites 和相关 metadata 会参与描述或运行时判断。Skill 还可以在 `metadata.config` 声明配置变量；anureo 会用配置值或 default 解析变量，并把解析结果以 `[Skill Configuration]` 区块注入 skill 内容。不要把 secrets 写入 skill 文件、references 或 prompt。

## 1. 先列出和检查 skills

### `anureo skills list`

列出默认 skill library 中的 skill，并显示 `auto`、`manual` 或 `evolved` source，以及 active、stale、archived lifecycle 标记：

```powershell
anureo skills list
anureo --json skills list
```

这个 CRUD 列表使用 CLI 的默认 skill registry；要理解一个名字在当前项目、profile、用户、agent 或 builtin 范围内到底来自哪里，使用 `inspect`。

### `anureo skills show <name>`

查看一个 skill 的 description、source、lifecycle、triggers 和 body：

```powershell
anureo skills show release-checklist
anureo --json skills show release-checklist
```

`show` 适合确认已保存的正文；它不是完整的 agent-discovery 视图。

### `anureo skills inspect <name>`

`inspect` 从 agent discovery perspective 展示 source、路径、description、triggers、条件、prerequisites、supporting files、embedded references、readiness、usage 和正文预览：

```powershell
anureo skills inspect release-checklist
anureo skills inspect release-checklist --all
anureo skills inspect release-checklist --read-file references/release.md
anureo skills inspect workflow --source Builtin
anureo --json --pretty skills inspect release-checklist --all
```

- 默认文本输出会截断 body；`--all` 展示完整 fields 和完整 body。
- `--read-file <PATH>` 读取该 skill 的 supporting file，例如 `references/api.md`。对磁盘-backed skill，路径必须留在 skill directory 内，且文件大小不能超过 5 MiB；`..` 或越界路径会被拒绝。Builtin skill 不读取磁盘：`PATH` 必须精确匹配其 embedded file name，因此不适用这条磁盘路径和大小检查。
- `--source <SOURCE>` 用于名字有多个候选项时消歧。可用值为 `Project`、`Profile`、`User`、`Agent`、`Data`、`Builtin`。源码中的 `ProfileDir` 对 CLI 参数使用 `Profile` 标签。
- `--json` 可与 `--pretty` 及全局 `--file <PATH>` 配合，便于保存机器可读检查结果。

推荐的复用前检查顺序是：先用 `inspect` 确认 source、path、triggers、conditions 和 supporting files，再决定是否接受、编辑或复制它。尤其要留意 Project/Agent 内容是否只适用于当前项目，以及 User/Data 内容是否会进入不相关的后续任务。

## 2. 创建、编辑和删除 skills

### `create`

创建一个 active、manual source 的 skill；`--description` 和可重复的 `--trigger` 用于初始 metadata：

```powershell
anureo skills create release-checklist `
  --description "发布前检查清单" `
  --trigger release `
  --trigger deploy
```

创建后用 `anureo skills show release-checklist` 检查保存结果，再用 `anureo skills inspect release-checklist` 确认它在当前 working folder 中如何被发现。若需要 `references/`、templates 或其它 supporting files，应在确认 skill directory 后再补充，并在正文中说明何时读取它们。

### `edit`

编辑会把 skill 的 raw 内容写入临时文件，使用 `$EDITOR`（Windows 下也读取同名环境变量；未设置时默认 `vi`）。当前 CLI 把环境变量的整个值当作一个可执行文件名传给 `Command::new`，不会解析命令参数；在 Windows 上应填写单一可执行文件名或完整路径。`notepad.exe` 会等待窗口关闭后再保存回 registry：

```powershell
$env:EDITOR = "notepad.exe"
anureo skills edit release-checklist
```

不应把 `code --wait` 作为 `$EDITOR` 值；它会被当成名为 `code --wait` 的可执行文件。若使用其它编辑器，请先确认该可执行文件本身会等待编辑结束；否则 CLI 可能在编辑完成前就读取临时文件。编辑器成功退出后才会保存，取消或失败则不会更新 skill。

编辑完成后重新运行 `show` 和 `inspect`，确认 frontmatter、触发词和 scope 没有意外改变。若要调整 skill 目录中的 `references/` 等文件，直接在已确认的 skill directory 内修改，并再次用 `--read-file` 复核。

### `delete`

删除前再次用 name 和 source/path 复核目标；`delete` 会删除 registry 中的 skill：

```powershell
anureo skills inspect release-checklist
anureo skills delete release-checklist
```

删除属于实际文件变更。它不会替你判断其它项目或 profile 是否仍需要同名资产，因此共享前先确认 source 和影响范围。

## 3. 触发词、目录和后续任务

触发词不是独立的命令入口，而是 skill metadata 中帮助 agent 判断何时加载 skill 的 signals。使用具体、稳定的词组，例如 `release`、`deploy`、`migration`；正文则写清适用条件、步骤和验证方式。把大段资料放到 `references/`，在正文中说明按需读取的文件名，避免每次任务都携带无关上下文。

Skill registry 会随任务发现 skills；profile 还可以通过 `skills.dirs` 增加目录，并通过 `skills.enabled`、`skills.disabled` 和平台过滤器筛选它们。一个 profile 的 skill 组合因此会改变 agent 后续任务看见的可复用上下文，但不会把所有 skill 自动变成项目规范。接受前仍应检查 source、path、triggers、conditions 和正文。

## 4. Agent profiles

### `anureo agent list` 与 `--agent`

列出可用的 agent profiles：

```powershell
anureo agent list
anureo --agent explore -m "定位认证流程和相关测试，不修改文件"
anureo --agent dev -m "实现修复并运行相关测试"
```

当前源码中的示例 profile 包括：`assistant`（通用对话和完整工具访问）、`dev`（开发角色，默认 `standard` tier）、`explore`（代码搜索角色，使用 `light` tier 并禁用写入、删除和 agent 等工具）、`agent-builder`（根据自然语言生成新的 profile）。实际可用列表以 `anureo agent list` 为准。

profile 会叠加到父配置：可以覆盖 model、tier 和 temperature，设置 working folder，加载 MCP config，拼接 role 内容和 `AGENTS.md`，加入 profile skills，并限制 builtin tools、平台和 sub-agent depth。未设置的字段通常继承父配置。换言之，`--agent` 不只是提示词别名，它会改变模型解析、工具权限、工作目录和可见 skills；执行修改任务前应检查 profile 的权限边界。

## 5. Export 到其它 agent 工具

`agent export` 把一个 profile 转换成第三方工具能读取的静态文件。格式只有 `claude-code`、`codex`、`cursor`：

```powershell
anureo agent export claude-code dev
anureo agent export codex dev --output .\exports
anureo agent export cursor explore --output .\exports
```

默认不指定 agent 时会导出所有 project agents；`--output <DIR>` 是输出目录，默认当前目录；`--dry-run` 不写文件，而是把每个目标路径和内容打印到 stdout：

```powershell
anureo agent export claude-code dev --output .\exports --dry-run
```

输出目标由格式决定：

| Format | 文件路径 | 内容形态 |
|---|---|---|
| `claude-code` | `.claude/agents/<NAME>.md` | YAML frontmatter、role body、constraints；会映射 model、tools、disallowedTools、maxTurns |
| `codex` | `.codex/agents/<NAME>.toml` | `name`、`description`、可选 `model` 和 `developer_instructions` |
| `cursor` | `.cursor/agents/<NAME>.md` | YAML frontmatter、role body 和 constraints |

导出会创建所需父目录并写文件；它是 profile 的转换结果，不是双向同步。修改第三方文件不会更新 anureo profile，后续重新 export 可能覆盖同名输出。

## 6. 实验性维护命令（不作为基础教程）

以下入口存在于 CLI，但属于实验性生命周期/遥测维护能力，不能代替创建、检查和人工审核：

- `skill-usage`：同步、查看或修复默认 skill registry 目录下的 `.usage.json`（当前默认位置通常是 `~/.anureo/data/skills/.usage.json`；使用 `sync`/`repair` 的 `--path` 时以显式目录为准）。例如 `anureo skill-usage sync --dry-run` 只预览新增 usage entries；`anureo skill-usage repair` 可能生成 `.bak` 并恢复损坏数据。
- `inspect` 的 `usage` 字段是另一条 best-effort 读取路径：它读取 `ANUREO_HOME/.skills.usage.json`，而不是 `skill-usage` 的 registry `.usage.json`。因此两者尚未统一，`inspect` 可能显示零值或没有数据，即使 `skill-usage` 的 store 已有记录；需要查看 curator registry usage 时使用 `anureo skill-usage show [<name>]`。
- `curator`：执行 stale/archive 等 lifecycle 管理和可选 LLM consolidation。它可能改变 skill 状态或归档内容，使用前先看其命令帮助和 dry-run/确认选项。
- `evolve`：CLI 中标记为 skill evolution 管理入口；当前不把它作为本指南的可用基础流程，也不描述其内部行为。

基础工作流保持简单：创建或发现 → `inspect` 确认 scope/source → `show` 或编辑正文 → 再次检查 → 在明确的 `--agent` profile 下运行后续任务。这样既能复用知识，也能避免把不适用的 project、user 或 agent 上下文静默带入下一次工作。
