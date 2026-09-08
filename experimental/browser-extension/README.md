# anureo Browser Extension

Browser automation for anureo Agent via MCP. Control any Chromium browser (Chrome, Edge, Brave) with screenshots, clicks, typing, and more.

## Architecture

```
anureo Agent ←stdio MCP→ mcp-server.js ←TCP 18765→ native-host.js ←Native Messaging→ Chrome Extension
```

- `extension/` — Chrome MV3 extension (background.js + content.js)
- `host/` — Native Messaging host + MCP server
- `install.ps1` / `install.sh` — One-time setup scripts

## Installation

### 1. Load the extension

1. Open `chrome://extensions`
2. Enable "Developer mode"
3. Click "Load unpacked" → select the `extension/` folder
4. Note the extension ID from the URL bar (e.g. `apmjddkjeolhmiaoinfkomlhogchcjdc`)

### 2. Register native messaging host

**Windows (PowerShell):**
```powershell
.\install.ps1 -ExtensionIds YOUR_EXTENSION_ID
```

**macOS/Linux:**
```bash
./install.sh YOUR_EXTENSION_ID
```

### 3. Restart browser

Close **all** browser windows and reopen. The extension needs a fresh start to connect to the native host.

### 4. Verify

When anureo Agent starts in a project with `.anureo/mcp.json`, it auto-connects. Look for the "MCP" tab group appearing in your browser.

## Available MCP Tools

| Tool | Description |
|------|-------------|
| `tabs_context_mcp` | List/manage MCP tab group |
| `tabs_create_mcp` | Create new tab in MCP group |
| `navigate` | Navigate to URL (supports "back"/"forward") |
| `computer` | Multi-action: screenshot, click, type, key, scroll, hover, drag, wait |
| `read_page` | Accessibility tree snapshot |
| `get_page_text` | Extract visible page text |
| `find` | Find elements by text/role |
| `form_input` | Set form field value by ref |
| `javascript_tool` | Execute JavaScript in page |
| `read_console_messages` | Read browser console |
| `read_network_requests` | Read network traffic |
| `resize_window` | Resize browser window |
| `upload_image` | Upload screenshot to file input |
| `gif_creator` | Record screen frames |

## Quick Example

Ask anureo Agent:
> "Open reddit.com in the browser and take a screenshot"

The agent will:
1. Call `tabs_context_mcp` with `createIfEmpty: true`
2. Call `navigate` to go to reddit.com
3. Call `computer` with `screenshot` action
4. Return the screenshot

## Credits

Based on [open-claude-in-chrome](https://github.com/noemica-io/open-claude-in-chrome) by noemica.io, adapted for anureo Agent.
