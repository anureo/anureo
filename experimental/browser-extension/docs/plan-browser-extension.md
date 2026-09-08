# anureo Browser Extension — 集成方案

> 基于 [noemica-io/open-claude-in-chrome](https://github.com/noemica-io/open-claude-in-chrome)，用 anureo 替换 Claude Code 作为 Agent 层。

---

## 1. 背景与目标

### 1.1 原项目架构

```
Claude Code ←stdio MCP→ mcp-server.js ←TCP 18765→ native-host.js ←Native Messaging→ Chrome Extension
```

三个组件：

| 组件 | 职责 | 代码量 |
|---|---|---|
| `extension/` | Chrome MV3 插件：CDP 浏览器控制、无障碍树、表单填写、截图等 | `background.js` ~940 行, `content.js` ~460 行 |
| `host/mcp-server.js` | MCP Server（stdio），注册 18 个工具，通过 TCP 转发到 native host | ~700 行 |
| `host/native-host.js` | Native Messaging Host，桥接 Chrome native messaging ↔ TCP | ~140 行 |

### 1.2 目标

将 Agent 层从 Claude Code 替换为 anureo，保留浏览器插件的全部能力。

### 1.3 约束

- 不修改 `extension/` 目录的任何代码（经过验证的稳定实现）
- 不修改 `host/` 目录的任何代码（通信协议已完善）
- 只需做 anureo 侧的配置集成

---

## 2. 方案

### 2.1 核心思路

原项目的 `mcp-server.js` 本身就是一个标准的 MCP Server，通过 stdio 通信。anureo 原生支持 MCP Server 注册。**只需将 `mcp-server.js` 注册为 anureo 的 MCP 工具源即可。**

```
anureo Agent ←stdio MCP→ mcp-server.js ←TCP 18765→ native-host.js ←Native Messaging→ Chrome Extension
                  ↑
              这一层不变，只是调用者从 Claude Code 换成 anureo
```

### 2.2 改动范围

仅 1 个文件：

```
.anureo/mcp.json    ← 新增，注册 MCP Server
```

无需改动任何现有代码。

### 2.3 配置

`.anureo/mcp.json`：

```json
{
  "mcpServers": {
    "browser": {
      "command": "node",
      "args": ["vendor/open-claude-in-chrome/host/mcp-server.js"],
      "cwd": "vendor/open-claude-in-chrome/host"
    }
  }
}
```

- `command`: Node.js 运行 MCP Server
- `args`: 指向原项目的 `mcp-server.js`
- `cwd`: 工作目录设为 `host/`，确保 `node_modules` 可被正确解析

---

## 3. 安装步骤

### 3.1 安装依赖

```bash
cd vendor/open-claude-in-chrome/host
npm install
```

依赖：`@modelcontextprotocol/sdk` ^1.12.1

### 3.2 加载 Chrome 扩展

1. 打开 `chrome://extensions`（或对应浏览器的扩展管理页）
2. 开启「开发者模式」
3. 点击「加载已解压的扩展程序」
4. 选择 `vendor/open-claude-in-chrome/extension/` 目录
5. 复制显示的扩展 ID

### 3.3 注册 Native Messaging Host

**macOS / Linux：**

```bash
cd vendor/open-claude-in-chrome
chmod +x install.sh
./install.sh <你的扩展ID>
```

**Windows：**

手动创建注册文件，或参考 `install.sh` 中的逻辑编写等效的 PowerShell 脚本。

注册文件路径：

| 浏览器 | macOS | Windows |
|---|---|---|
| Chrome | `~/Library/Application Support/Google/Chrome/NativeMessagingHosts/` | 注册表 `HKCU\Software\Google\Chrome\NativeMessagingHosts\` |
| Edge | `~/Library/Application Support/Microsoft Edge/NativeMessagingHosts/` | 注册表 `HKCU\Software\Microsoft\Edge\NativeMessagingHosts\` |
| Brave | `~/Library/Application Support/BraveSoftware/Brave-Browser/NativeMessagingHosts/` | 注册表 `HKCU\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\` |

### 3.4 重启浏览器

关闭所有窗口后重新打开。浏览器在启动时读取 native messaging 配置。

### 3.5 配置 anureo

确保 `.anureo/mcp.json` 已按 2.3 节配置。anureo 启动时会自动启动 MCP Server。

---

## 4. 可用工具清单

anureo 注册成功后，Agent 可使用以下 18 个 MCP 工具：

| 工具 | 功能 | 参数 |
|---|---|---|
| `tabs_context_mcp` | 获取 MCP Tab 分组上下文 | `createIfEmpty?` |
| `tabs_create_mcp` | 创建新 Tab | — |
| `navigate` | 导航到 URL / 前进 / 后退 | `url`, `tabId` |
| `computer` | 鼠标、键盘、截图（13 种动作） | `action`, `tabId`, `coordinate?`, `text?`, ... |
| `read_page` | 无障碍树（含元素 ref 引用） | `tabId`, `filter?`, `depth?`, `max_chars?`, `ref_id?` |
| `get_page_text` | 提取页面正文 | `tabId` |
| `find` | 自然语言查找元素 | `query`, `tabId` |
| `form_input` | 通过 ref 设置表单值 | `ref`, `value`, `tabId` |
| `javascript_tool` | 在页面中执行 JS | `text`, `tabId` |
| `read_console_messages` | 读取浏览器控制台日志 | `tabId`, `pattern?`, `limit?`, `onlyErrors?`, `clear?` |
| `read_network_requests` | 读取网络请求 | `tabId`, `urlPattern?`, `limit?`, `clear?` |
| `resize_window` | 调整窗口大小 | `width`, `height`, `tabId` |
| `upload_image` | 上传截图到文件输入 | `imageId`, `tabId`, `ref?`, `coordinate?` |
| `gif_creator` | GIF 录制（stub） | `action`, `tabId` |
| `shortcuts_list` | 列出快捷方式（stub） | `tabId` |
| `shortcuts_execute` | 执行快捷方式（stub） | `tabId`, `shortcutId?` |
| `switch_browser` | 切换浏览器（stub） | — |
| `update_plan` | 展示操作计划（自动通过） | `domains`, `approach` |

---

## 5. 使用流程

### 5.1 典型会话

```
1. Agent 调用 tabs_context_mcp({ createIfEmpty: true })
   → 创建 MCP Tab 分组，返回 tab ID 列表

2. Agent 调用 navigate({ url: "https://example.com", tabId: 123 })
   → 在指定 Tab 中导航

3. Agent 调用 computer({ action: "screenshot", tabId: 123 })
   → 截图，返回 base64 图片

4. Agent 调用 read_page({ tabId: 123, filter: "interactive" })
   → 获取无障碍树，查看可交互元素及其 ref

5. Agent 调用 form_input({ ref: "ref_5", value: "hello", tabId: 123 })
   → 填写表单

6. Agent 调用 computer({ action: "left_click", coordinate: [100, 200], tabId: 123 })
   → 点击按钮
```

### 5.2 与 anureo 内置工具的关系

anureo 已有内置的浏览器控制工具（`take_snapshot`, `click`, `navigate_page` 等），通过 CDP 直接操作浏览器。MCP 工具提供了额外能力：

| 能力 | anureo 内置 | MCP 工具 | 建议使用 |
|---|---|---|---|
| 页面导航 | `navigate_page` | `navigate` | 内置（更直接） |
| 截图 | `take_screenshot` | `computer(screenshot)` | 内置（更直接） |
| 点击/键盘 | `click`, `type_text`, `press_key` | `computer(left_click/type/key)` | 内置（更直接） |
| JS 执行 | `evaluate_script` | `javascript_tool` | 内置（更直接） |
| Console 监控 | `list_console_messages` | `read_console_messages` | 内置（更直接） |
| Network 监控 | `list_network_requests` | `read_network_requests` | 内置（更直接） |
| 窗口大小 | `resize_page` | `resize_window` | 内置（更直接） |
| *无障碍树* | `take_snapshot`（a11y tree） | `read_page` | MCP（含 ref 引用系统） |
| *元素查找* | — | `find` | 仅 MCP |
| *表单填写* | `fill`（基础） | `form_input`（shadow DOM 穿透） | MCP（更智能） |
| *正文提取* | — | `get_page_text` | 仅 MCP |
| *Tab 分组* | — | `tabs_context_mcp` | 仅 MCP |

**建议**：基础操作用 anureo 内置工具（更快），需要 ref 引用、shadow DOM 穿透、正文提取时用 MCP 工具。

---

## 6. 多会话支持

`mcp-server.js` 内置了 PRIMARY / CLIENT 模式：

- 第一个 anureo 会话成为 PRIMARY，拥有 TCP 端口
- 后续会话自动以 CLIENT 模式连接到 PRIMARY
- 所有会话共享同一个浏览器扩展

无需额外配置。

---

## 7. 故障排除

### 7.1 MCP Server 启动失败

```bash
# 检查依赖是否安装
cd vendor/open-claude-in-chrome/host && npm install

# 手动测试 MCP Server
node mcp-server.js
# 应输出 "Primary MCP server listening on :18765" 或 "Port 18765 in use. Connecting as client..."
```

### 7.2 "Browser extension is not connected"

1. 确认扩展已加载并启用
2. 确认 `install.sh` 使用了正确的扩展 ID
3. 重启浏览器（关闭所有窗口）
4. 检查 native host wrapper 是否存在：`vendor/open-claude-in-chrome/host/native-host-wrapper.sh`

### 7.3 端口冲突

默认 TCP 端口 18765。如需修改：

```bash
mkdir -p ~/.config/open-claude-in-chrome
echo '{"port": 19000}' > ~/.config/open-claude-in-chrome/config.json
```

重启浏览器和 anureo。

### 7.4 清理残留进程

```bash
# macOS / Linux
pkill -f "node.*mcp-server"

# Windows
taskkill /F /FI "WINDOWTITLE eq mcp-server*"
```

---

## 8. 优化方向（实施状态）

1. ~~**Windows 安装脚本**~~ — ✅ 已实现 `install.ps1`，支持 Chrome/Edge/Brave 注册
2. ~~**Agent Profile**~~ — ✅ 已创建 `.anureo/agents/browser/profile.md`
3. ~~**Skill**~~ — ✅ 已创建 `.anureo/skills/browser-automation.md`
4. **WebSocket 方案** — 待定，当前 Native Messaging 方案可工作
5. ~~**GIF 录制**~~ — ✅ 已实现 `gif_creator` 工具（start/stop/export/clear）
6. ~~**图片上传**~~ — ✅ 已实现 `upload_image`（支持 ref 和 coordinate 两种方式）
7. ~~**品牌重命名**~~ — ✅ 从 "Open Claude in Chrome" 重命名为 "anureo Browser"

---

## 附录 A：WebSocket 替代方案（未实施）

### 动机

Native Messaging 需要系统级注册（写文件/注册表），安装步骤多。WebSocket 可以简化。

### 架构

```
anureo Agent ←stdio MCP→ browser-mcp-server.js ←WebSocket→ Extension background.js → content.js
```

### 需要改动的文件

| 文件 | 改动 |
|---|---|
| `background.js` | 将 Native Messaging 替换为 WebSocket Client |
| `manifest.json` | 去掉 `nativeMessaging` 权限 |
| `browser-mcp-server.js` | 新写：MCP stdio + WebSocket Server |
| `host/native-host.js` | 删除 |
| `install.sh` | 删除 |

### 安全措施

- Token 认证：MCP Server 生成随机 token，WS 连接需携带
- 仅监听 localhost
- Token 通过文件传递（权限 600）

### 未实施原因

- 原方案代码已验证稳定，零改动集成风险最低
- WebSocket 方案需要重写 background.js 和 MCP Server，引入新风险
- 可在验证原方案可行后再考虑迁移

### 安全性对比

| | Native Messaging | WebSocket + Token |
|---|---|---|
| 认证 | Chrome 校验 extension ID + binary 路径 | 自定义 token |
| 攻击面 | 无（进程间管道） | localhost WS 可被同机网页连接 |
| 适用场景 | 生产环境 | 本地开发 |
