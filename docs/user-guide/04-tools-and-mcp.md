# anureo 工具与 MCP

> **状态**：已实现；本文只描述当前稳定 CLI 暴露的工具检查、MCP 配置与生命周期管理。

本文面向希望让 Agent 使用文件、shell、browser、MCP 或其他工具的开发者。目标是建立一条可审计路径：先确认工具定义和输入，再确认影响范围与授权，最后配置、启停并诊断 MCP server。示例使用 PowerShell；`anureo` 也可以替换为 `cargo run -p anureo-cli --`（Cargo 参数前的 `--` 是分隔符）。

## 1. 前提：先确认边界

从项目根目录开始，并明确 Agent 的 working folder：

```powershell
anureo --working-folder . -m "只读检查当前项目结构，不修改文件"
```

未指定 `--working-folder` 时使用当前目录。目录必须存在且必须是目录。默认情况下（`allow_paths_outside_workdir=false`），文件工具会以该目录的 canonical path 做路径包含检查；这是文件工具的工作边界。若配置将 `allow_paths_outside_workdir` 设为 `true`，运行时会把该选项传给所有文件工具，允许访问工作目录之外的路径，因此 canonical path 不再构成安全边界，越界读写、移动或删除的影响范围也会扩大。调用工具前应记录当前目录、任务目标和允许的副作用。高风险修改可以使用隔离 worktree：

```powershell
anureo --worktree --working-folder . -m "修改并测试此功能，完成后报告 diff"
```

`--worktree` 不是回滚机制；取消运行也不会撤销已经完成的外部操作。完成后仍要检查 Git status、diff 和未跟踪文件。

## 2. 从发现到确认：检查工具定义

### 2.1 列出工具

先看本次运行实际加载的工具名称和 description：

```powershell
anureo tool list
```

这是工具发现的入口，不应凭记忆猜测工具名。`tool list` 输出的是当前构建的 registry 内容；它可能包含 builtin tools 和已加载的 MCP tools。若需要脚本消费，可使用全局 `--json`：

```powershell
anureo --json tool list
```

### 2.2 查看完整 definition

用 `tool show NAME` 检查单个工具的 name、description 和 `input_schema`：

```powershell
anureo tool show read
anureo tool show write_file --output yaml
anureo tool show write_file --output json
```

`--output` 只接受 `yaml`（默认）或 `json`；全局 `--json` 会使 `tool show` 使用 JSON。YAML/JSON definition 是调用前的审计材料：记录必填字段、字段类型、默认值、枚举、路径/URL 等范围字段，以及 description 中描述的限制。

推荐的确认顺序是：

1. 用 `anureo tool list` 找到候选工具。
2. 用 `anureo tool show NAME --output json` 保存或查看 definition。
3. 阅读 `input_schema`，确认输入是否包含路径、命令、URL、上传内容、删除/覆盖开关等高影响参数。
4. 将工具、动作、目标范围和授权时长写入确认记录；不清楚范围时先拒绝调用或缩小范围。
5. 调用后复核输出和实际状态。Agent 报告完成只表示运行结束，不表示文件、远端数据或业务结论正确。

`tool show` 失败通常表示名称不在当前 registry；先重新运行 `anureo tool list`，确认是否拼写错误、工具源未加载，或工具属于未启用的配置。

## 3. 稳定工具的副作用

当前稳定工具源包含文件工具、bash、web fetcher 和 MCP adapter 等。`tool list/show` 是能力确认入口，但 definition 不是安全批准本身：输入内容、目标范围和运行时环境仍需人工判断。

| 工具类别 | 可能影响 | 调用前至少确认 |
| --- | --- | --- |
| 文件读取、glob、grep | 暴露源码、配置或个人数据 | working folder、路径是否越界、输出是否会进入日志或模型上下文 |
| 文件写入、edit、multiedit、apply patch、move、delete、create dir | 修改、覆盖、移动或删除本地文件 | 目标路径、覆盖/删除范围、Git 状态、是否先用 `--worktree` |
| shell / bash | 执行任意命令，可能安装依赖、删除数据、发布或联网 | command、cwd、参数、网络、权限、管道和恢复方式 |
| browser / web fetcher | 读取远端内容；提交表单、上传或登录流程可能产生远端副作用 | 站点、URL、登录状态、提交动作、上传文件和接收方 |
| MCP | 由第三方 server 决定的工具集合，可能同时读写本地和远端资源 | server 来源、command/URL、headers、environment、网络和每个暴露工具的 schema |

不要把“只读任务”当作绝对保证：网页可能变化，MCP server 可能暴露写入工具，shell 也可能包含隐含副作用。对删除、发布、表单提交、上传、安装和写入操作逐次确认。

**实验性边界**：`agent/tool/tool-experimental` 中导出的 memory、task、LLM 等模块不是本章的稳定 CLI 工具教程；不要依据这些 Rust export 推断 `anureo tool` 已公开对应命令。本文也不介绍实现 MCP server 的 SDK、ACP 反向 fs/terminal bridge 或未在稳定 CLI 中暴露的实验工具。

## 4. MCP 配置文件与发现

anureo 使用 Cursor/Claude-compatible 的 JSON 根对象：

```json
{
  "mcpServers": {
    "server-name": {
      "command": "...",
      "args": ["..."],
      "env": {"NAME": "replace-with-a-secret"},
      "disabled": false
    }
  }
}
```

运行时的可用配置发现顺序是：

```text
--mcp-config PATH（仅当 PATH 存在时作为 override）
  > 项目 working folder/.anureo/mcp.json
  > $ANUREO_HOME/mcp.json（默认 ~/.anureo/mcp.json）
```

`--mcp-config` 是全局 run option：

```powershell
anureo --mcp-config .\.anureo\readonly-mcp.json -m "只读列出可用 MCP 工具，不修改文件"
```

如果显式路径不存在，底层 discovery 会继续查找项目和全局路径。没有发现文件时，Agent 运行路径不会凭空得到一个有效 server；而 `anureo mcp ...` 管理器在没有配置文件时会创建默认的用户级 `mcp.json`。本章稳定的显式覆盖方式是 `--mcp-config`；不要把管理器的发现结果误认为本次 run 一定使用了同一文件。

每个 server entry 必须有 `command` 或 `url`。`disabled: true` 的 entry 会保留在配置和 `anureo mcp show` 中，但解析成运行定义时会被跳过。若同时写入 `url` 和 `command`，`url` 优先。非法 JSON、缺少两者、空 command 或非 `http://`/`https://` URL 都会导致配置错误。

## 5. 配置 stdio MCP server

stdio server 由 `command` 启动，`args` 按数组元素传参，`env` 为该 server 提供环境变量。用 CLI 添加时，`--arg` 和 `--env` 可重复：

```powershell
anureo mcp add --name local-tools `
  --command npx `
  --arg -y `
  --arg '@vendor/mcp-server' `
  --arg 'C:\work\project' `
  --env 'MCP_MODE=readonly' `
  --disabled
```

这里的包名、目录和环境值都是占位符；先审查将要运行的 command 和每个 arg，再 enable。`--disabled` 让新 entry 以 disabled 状态保存，不会被 MCP parser 加载。

等价的 JSON 结构是：

```json
{
  "mcpServers": {
    "local-tools": {
      "command": "npx",
      "args": ["-y", "@vendor/mcp-server", "C:\\work\\project"],
      "env": {"MCP_MODE": "readonly"},
      "disabled": true
    }
  }
}
```

`env` 的值会传给该 stdio process；它不是安全存储。避免在命令行、JSON、prompt、memory、skill 或 logs 中放真实 credential。`anureo mcp show` 的文本输出会对 environment value 做 masking，但 HTTP headers 会原样显示，不能把 headers 当作已脱敏边界。

## 6. 配置 HTTP MCP server

HTTP server 使用 `--url URL`，只接受 `http://` 或 `https://`：

```powershell
anureo mcp add --name remote-tools --url https://mcp.example.invalid/endpoint --disabled
anureo mcp show remote-tools
anureo mcp enable remote-tools
```

URL 是远端服务地址；命令行管理器创建的 entry 默认没有 headers 或 OAuth 字段。若服务要求 headers，应在审计过的 `mcp.json` 中补充：

```json
{
  "mcpServers": {
    "remote-tools": {
      "url": "https://mcp.example.invalid/endpoint",
      "headers": {
        "X-Environment": "replace-with-a-non-secret-label",
        "Authorization": "Bearer replace-with-a-secret"
      },
      "disabled": true
    }
  }
}
```

示例中的 header 值不可直接使用。审查 URL 的域名、TLS、数据出境、服务方保留策略和 headers 的权限，再 enable。HTTP 配置也可以包含源码支持的 `oauth` 对象；本文不提供 credential 或 API key 示例值。

## 7. MCP server 生命周期与配置管理

管理命令作用于 manager 发现到的配置文件：项目 `.anureo/mcp.json` 优先，其次 `$ANUREO_HOME/mcp.json`；没有文件时会创建用户级文件。先确认文件位置和 Git 状态，再执行写操作：

```powershell
anureo mcp list
anureo mcp show local-tools
anureo --json mcp list
anureo --json mcp show local-tools
```

`list` 显示 name、类型、disabled 状态以及 command 或 URL；`show` 还显示 args、脱敏后的 environment 和 headers。`--json` 输出机器可读 JSON，适合保存审计快照，但 headers 仍需按敏感信息处理。

生命周期操作如下：

```powershell
# 新建：必须提供 --command 或 --url；可用 --disabled 初始停用
anureo mcp add --name local-tools --command npx --arg -y --arg '@vendor/mcp-server' --disabled

# 修改：只传入的字段更新；未传字段保留原值
anureo mcp edit local-tools --command node --arg server.js --env 'MODE=readonly' --disabled true
anureo mcp edit remote-tools --url https://mcp.example.invalid/endpoint --disabled false

# 启停
anureo mcp enable local-tools
anureo mcp disable local-tools

# 删除：从配置文件移除 entry，不会替你清理已安装的包、远端资源或副作用
anureo mcp delete local-tools
```

注意：`edit` 的 `--args`/`--env` 只有在本次提供值时才替换对应数组/map；未提供时保留原值。`edit` 不提供清除单个旧 env/header 的专用参数；需要精确清理时先备份并审查 JSON。`add` 同时提供 `--command` 和 `--url` 时，CLI 优先选择 `--command`，写入 stdio entry（`command` 有值、`url` 为 `None`）；不要把 `add` 当作会生成混合字段的方式。若手工编辑 JSON 或通过合并产生同时含有 `command` 与 `url` 的 entry，parser 才会按 `url` 优先处理该混合 entry。

`enable`/`disable` 只是修改 `disabled` 字段，不代表已完成连通性检查，也不表示某个已经运行的 Agent 会热加载新状态。下一次运行加载配置时，disabled entry 才会被过滤。删除或启用第三方 server 前，重新运行 `anureo mcp show NAME` 并复核 command、URL、headers 和 environment。

## 8. 第三方 MCP 的 network review

把第三方 MCP 当成一个可执行或可联网的权限边界，至少审查：

- `command`、全部 `args`、包来源、版本锁定方式以及它是否会安装或执行额外代码。
- stdio 的 `env`：名称、值来源、是否能访问文件系统/云资源，以及是否会被子进程继承。
- HTTP 的 URL：域名、协议、路径、数据发送范围、TLS、重定向和服务方日志/保留策略。
- `headers` 与 OAuth：权限、有效期、是否会出现在日志/错误中；不要把真实值复制到 issue 或文档。
- server 返回的所有 tools：逐项运行 `anureo tool list` 与 `anureo tool show NAME --output json`，检查 input schema 和写入/删除/发布能力。
- 启用时段与撤销方式：优先一次性或短时授权，任务结束后 `anureo mcp disable NAME`。

先以 disabled 配置保存、查看和审计，再 enable；如果没有可用的确认机制，不要让高风险 MCP 调用静默执行。

## 9. 错误诊断

### 工具被 denied

确认工具是否存在以及输入是否超出边界：

```powershell
anureo tool list
anureo tool show NAME --output json
anureo -vvv --working-folder . -m "只读说明为何需要 NAME；不要调用写入工具"
```

检查 working folder、目标路径、操作类型和当前授权策略。文件越界、删除、写入、shell 安装/发布、browser submit/upload 等都应缩小范围或重新确认；不要为了绕过 denied 而改用语义相近但影响更大的工具。

### MCP unavailable

按以下顺序保留脱敏证据：

```powershell
anureo mcp list
anureo mcp show NAME
anureo --mcp-config .\.anureo\mcp.json -vvv -m "只读检查 MCP 是否可用，不执行写入"
```

核对实际文件路径、JSON 是否可解析、entry 是否 `disabled`、stdio command/args 是否能在目标环境启动、env 是否完整，以及 HTTP URL、网络、headers 和远端权限。`anureo mcp list` 只证明管理器读到了配置，不证明 server 已连接或其 tools 可用。

### 配置错误与未知工具

缺少 `command`/`url`、空 command、非法 JSON 或非 HTTP(S) URL 会在读取/解析阶段失败。先用最小 entry 修复配置，再逐个启用 server。若 server 成功加载但工具名不存在，重新运行 `anureo tool list`；MCP tools 的名字和数量由 server 返回，不应从 server 名称猜测。

收集诊断时可使用明确的 log 文件，但提交前要脱敏：

```powershell
anureo --log-level debug --log-file .\.anureo\logs\tools-diagnostic.log -m "执行一个最小只读检查"
```

日志、tool output、路径、HTTP metadata 和错误上下文都可能包含敏感信息。修复后再次检查 `git status --short`，确认没有把 `.env`、`mcp.json`、日志或生成文件意外提交。
