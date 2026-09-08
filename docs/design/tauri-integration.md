# Tauri 桌面应用集成方案

> **状态**: 提案
> **日期**: 2025-08-19
> **范围**: 将 anureo Web 应用转换为 Tauri 桌面应用
> **相关仓库**: `anureo` (后端) / `anureo-feat-dev` (前端)

## 1. 背景与目标

### 1.1 现状分析

当前 anureo (anureo) 作为纯 Web 应用运行在浏览器中，通过 ACP WebSocket 协议与 anureo 后端通信。虽然功能完整，但缺乏桌面应用的系统集成能力。

**现有架构**:
```
浏览器 ←→ ACP WebSocket ←→ anureo Backend (3031端口)
```

**局限性**:
- 无法实现系统托盘和菜单栏集成
- 文件关联和拖放支持有限
- 系统通知依赖浏览器权限
- 启动需要打开浏览器并访问特定URL
- 无法直接访问底层系统资源

### 1.2 目标

通过 Tauri 将 anureo 打包为原生桌面应用，提供：

1. **原生桌面体验**: 系统托盘、菜单栏、文件关联
2. **更好的系统集成**: 原生通知、快捷键、窗口管理
3. **简化的分发**: 单一可执行文件，无需浏览器依赖
4. **保持架构**: 维持前后端分离，ACP 协议不变

**目标架构**:
```
Tauri Desktop App ←→ ACP WebSocket ←→ anureo Backend
```

## 2. 技术方案

### 2.1 整体架构

```
┌─────────────────────────────────────┐
│        Tauri Desktop Application     │
│  ┌─────────────────────────────┐    │
│  │   Frontend (React/Vue)      │    │
│  │   - 现有 anureo UI     │    │
│  │   - Vite 构建               │    │
│  └──────────┬──────────────────┘    │
│             │ Tauri IPC             │
│  ┌──────────▼──────────────────┐    │
│  │   Tauri Backend (Rust)      │    │
│  │   - 窗口管理                │    │
│  │   - 系统集成                │    │
│  │   - ACP WebSocket 客户端    │    │
│  └──────────┬──────────────────┘    │
└─────────────┼───────────────────────┘
              │ ACP WebSocket
              ▼
┌─────────────────────────────────────┐
│        anureo Backend Server          │
│  - HTTP + ACP WebSocket (3031)      │
│  - Agent 运行时                     │
│  - 会话管理                         │
└─────────────────────────────────────┘
```

### 2.2 项目结构

```
anureo-feat-dev/
├── src-tauri/                    # Tauri Rust 后端
│   ├── src/
│   │   ├── main.rs              # Tauri 入口点
│   │   ├── lib.rs               # Tauri API 命令
│   │   ├── commands/            # Tauri 命令模块
│   │   │   ├── window.rs        # 窗口管理
│   │   │   ├── system.rs        # 系统集成
│   │   │   ├── tray.rs          # 系统托盘
│   │   │   └── notification.rs  # 通知管理
│   │   ├── acp/                 # ACP 协议实现
│   │   │   ├── client.rs        # WebSocket 客户端
│   │   │   ├── protocol.rs      # ACP 协议处理
│   │   │   └── reconnect.rs     # 重连逻辑
│   │   └── config.rs            # 配置管理
│   ├── Cargo.toml
│   ├── tauri.conf.json          # Tauri 配置
│   ├── build.rs                 # 构建脚本
│   └── icons/                   # 应用图标
│       ├── 32x32.png
│       ├── 128x128.png
│       ├── 128x128@2x.png
│       └── icon.icns
├── packages/                     # 现有前端代码
│   ├── ui/                      # UI 组件
│   ├── web/                     # Web 应用
│   └── ...
├── package.json
├── vite.config.ts
└── tsconfig.json
```

### 2.3 核心技术栈

**前端**:
- 框架: 保持现有 (React/Vue/Svelte)
- 构建工具: Vite (已使用)
- UI 库: 保持现有组件库

**后端**:
- 语言: Rust (与 anureo 后端一致)
- 框架: Tauri 2.x
- WebSocket: tokio-tungstenite
- 异步运行时: tokio

**通信**:
- 前后端: Tauri IPC (invoke/invoke-handler)
- 与 anureo: ACP WebSocket (保持现有协议)

## 3. 核心功能实现

### 3.1 窗口管理

```rust
// src-tauri/src/commands/window.rs
use tauri::{Manager, Window};

#[tauri::command]
async fn create_window(window: Window, label: &str, title: &str, url: &str) -> Result<(), String> {
    let new_window = tauri::WindowBuilder::new(
        &window.app_handle(),
        label,
        tauri::WindowUrl::External(url.parse().unwrap())
    )
    .title(title)
    .inner_size(800.0, 600.0)
    .center()
    .build()
    .map_err(|e| e.to_string())?;
    
    Ok(())
}

#[tauri::command]
async fn focus_window(window: Window) -> Result<(), String> {
    window.set_focus().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn minimize_window(window: Window) -> Result<(), String> {
    window.set_minimized(true).map_err(|e| e.to_string())?;
    Ok(())
}
```

### 3.2 系统托盘

```rust
// src-tauri/src/commands/tray.rs
use tauri::{AppHandle, Manager};
use tauri::SystemTray;
use tauri::SystemTrayMenu;
use tauri::SystemTrayMenuItem;
use tauri::CustomMenuItem;

pub fn create_system_tray() -> SystemTray {
    let quit = CustomMenuItem::new("quit".to_string(), "退出");
    let hide = CustomMenuItem::new("hide".to_string(), "隐藏");
    let show = CustomMenuItem::new("show".to_string(), "显示");
    let start_server = CustomMenuItem::new("start_server".to_string(), "启动 anureo 服务");
    let stop_server = CustomMenuItem::new("stop_server".to_string(), "停止 anureo 服务");
    
    let tray_menu = SystemTrayMenu::new()
        .add_item(show)
        .add_item(hide)
        .add_native_item(SystemTrayMenuItem::Separator)
        .add_item(start_server)
        .add_item(stop_server)
        .add_native_item(SystemTrayMenuItem::Separator)
        .add_item(quit);
    
    SystemTray::new().with_menu(tray_menu)
}

pub fn handle_system_tray_event(app: &AppHandle, event: tauri::SystemTrayEvent) {
    match event {
        tauri::SystemTrayEvent::LeftClick { .. } => {
            let window = app.get_window("main").unwrap();
            window.show().unwrap();
            window.set_focus().unwrap();
        }
        tauri::SystemTrayEvent::MenuItemClick { id, .. } => {
            match id.as_str() {
                "quit" => {
                    std::process::exit(0);
                }
                "hide" => {
                    let window = app.get_window("main").unwrap();
                    window.hide().unwrap();
                }
                "show" => {
                    let window = app.get_window("main").unwrap();
                    window.show().unwrap();
                    window.set_focus().unwrap();
                }
                "start_server" => {
                    // 启动 anureo 服务
                }
                "stop_server" => {
                    // 停止 anureo 服务
                }
                _ => {}
            }
        }
        _ => {}
    }
}
```

### 3.3 ACP WebSocket 客户端

```rust
// src-tauri/src/acp/client.rs
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio::net::TcpStream;
use tokio_tungstenite::connector::TlsConnector;
use futures_util::{StreamExt, SinkExt};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct AcpClient {
    url: String,
    sender: Arc<Mutex<Option<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>>>>,
    message_handlers: Arc<Mutex<Vec<Box<dyn Fn(Value) + Send + Sync>>>>,
}

impl AcpClient {
    pub async fn new(url: String) -> Result<Self, String> {
        let url_with_ws = if url.starts_with("http://") {
            url.replace("http://", "ws://")
        } else if url.starts_with("https://") {
            url.replace("https://", "wss://")
        } else {
            url.clone()
        };
        
        let (ws_stream, _) = connect_async(&url_with_ws)
            .await
            .map_err(|e| format!("连接失败: {}", e))?;
        
        Ok(Self {
            url: url_with_ws,
            sender: Arc::new(Mutex::new(Some(ws_stream))),
            message_handlers: Arc::new(Mutex::new(Vec::new())),
        })
    }
    
    pub async fn send_request(&self, method: &str, params: Value) -> Result<Value, String> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": uuid::Uuid::new_v4().to_string(),
            "method": method,
            "params": params
        });
        
        let mut sender = self.sender.lock().await;
        if let Some(ws_stream) = sender.as_mut() {
            ws_stream.send(Message::Text(request.to_string()))
                .await
                .map_err(|e| format!("发送失败: {}", e))?;
            
            // 等待响应
            if let Some(message) = ws_stream.next().await {
                match message {
                    Ok(Message::Text(text)) => {
                        let response: Value = serde_json::from_str(&text)
                            .map_err(|e| format!("解析响应失败: {}", e))?;
                        return Ok(response);
                    }
                    Ok(_) => return Err("意外的消息类型".to_string()),
                    Err(e) => return Err(format!("接收消息失败: {}", e)),
                }
            }
        }
        
        Err("连接已关闭".to_string())
    }
    
    pub async fn start_message_loop(&self) {
        let mut sender = self.sender.lock().await;
        if let Some(ws_stream) = sender.take() {
            let handlers = self.message_handlers.clone();
            
            tokio::spawn(async move {
                let mut ws_stream = ws_stream;
                while let Some(message) = ws_stream.next().await {
                    match message {
                        Ok(Message::Text(text)) => {
                            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                                let handlers = handlers.lock().await;
                                for handler in handlers.iter() {
                                    handler(value.clone());
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("消息接收错误: {}", e);
                            break;
                        }
                    }
                }
            });
        }
    }
    
    pub fn on_message(&self, handler: Box<dyn Fn(Value) + Send + Sync>) {
        let mut handlers = self.message_handlers.blocking_lock();
        handlers.push(handler);
    }
}
```

### 3.4 文件关联

```rust
// src-tauri/src/commands/system.rs
use tauri::Manager;

#[tauri::command]
async fn open_file_in_project(app_handle: tauri::AppHandle, file_path: String) -> Result<(), String> {
    // 解析文件路径，确定所属项目
    let project_path = extract_project_path(&file_path)?;
    
    // 通过 ACP 打开项目
    let window = app_handle.get_window("main").unwrap();
    window.emit("open-project", &project_path)
        .map_err(|e| e.to_string())?;
    
    Ok(())
}

fn extract_project_path(file_path: &str) -> Result<String, String> {
    // 实现项目路径解析逻辑
    // 向上查找 .anureo 目录或项目标识文件
    let path = std::path::Path::new(file_path);
    
    // 简化实现：直接返回父目录
    if let Some(parent) = path.parent() {
        Ok(parent.to_string_lossy().to_string())
    } else {
        Err("无法确定项目路径".to_string())
    }
}

#[tauri::command]
async fn register_file_associations() -> Result<(), String> {
    // 注册文件关联（Windows）
    #[cfg(target_os = "windows")]
    {
        // 使用 Windows API 注册 .anureo 文件关联
        // 这里需要调用系统 API 或使用第三方库
    }
    
    // 注册文件关联（macOS）
    #[cfg(target_os = "macos")]
    {
        // 使用 macOS Info.plist 注册文件关联
    }
    
    // 注册文件关联（Linux）
    #[cfg(target_os = "linux")]
    {
        // 创建 .desktop 文件和 MIME 类型关联
    }
    
    Ok(())
}
```

### 3.5 系统通知

```rust
// src-tauri/src/commands/notification.rs
use tauri::Manager;

#[tauri::command]
async fn show_notification(
    app_handle: tauri::AppHandle,
    title: String,
    body: String,
    icon: Option<String>
) -> Result<(), String> {
    let notification = tauri::api::notification::Notification::new(&app_handle.config().tauri.bundle.identifier)
        .title(&title)
        .body(&body);
    
    if let Some(icon_path) = icon {
        notification.icon(tauri::api::path::resolve_path(&app_handle.config(), &icon_path, None)?);
    }
    
    notification.show()
        .map_err(|e| e.to_string())?;
    
    Ok(())
}

#[tauri::command]
async fn show_agent_completion_notification(
    app_handle: tauri::AppHandle,
    agent_name: String,
    session_id: String,
    result: String
) -> Result<(), String> {
    let title = format!("Agent {} 完成", agent_name);
    let body = format!("会话 {} 已完成", session_id);
    
    show_notification(app_handle, title, body, Some("icons/128x128.png".to_string())).await
}

#[tauri::command]
async fn show_error_notification(
    app_handle: tauri::AppHandle,
    error_message: String
) -> Result<(), String> {
    show_notification(
        app_handle, 
        "错误".to_string(), 
        error_message, 
        Some("icons/128x128.png".to_string())
    ).await
}
```

## 4. 配置文件

### 4.1 Tauri 配置

```json
{
  "$schema": "https://schema.tauri.app/config/2",
  "productName": "Anureo",
  "version": "1.0.0",
  "identifier": "com.anureo.app",
  "build": {
    "distDir": "../dist",
    "devPath": "http://localhost:5180",
    "beforeDevCommand": "npm run dev",
    "beforeBuildCommand": "npm run build",
    "withGlobalTauri": true
  },
  "app": {
    "windows": [
      {
        "title": "Anureo",
        "width": 1200,
        "height": 800,
        "resizable": true,
        "fullscreen": false,
        "center": true,
        "decorations": true,
        "transparent": false,
        "alwaysOnTop": false,
        "skipTaskbar": false,
        "theme": "system"
      }
    ],
    "security": {
      "csp": null,
      "assetProtocol": {
        "enable": true,
        "scope": ["**"]
      }
    }
  },
  "bundle": {
    "active": true,
    "targets": "all",
    "icon": [
      "icons/32x32.png",
      "icons/128x128.png",
      "icons/128x128@2x.png",
      "icons/icon.icns",
      "icons/icon.ico"
    ],
    "identifier": "com.anureo.app",
    "publisher": "Anureo Team",
    "copyright": "Copyright © 2025 Anureo Team",
    "category": "DeveloperTool",
    "shortDescription": "AI Agent Desktop Application",
    "longDescription": "Anureo is a desktop application for managing AI agents and development workflows.",
    "macOS": {
      "frameworks": [],
      "minimumSystemVersion": "10.13",
      "exceptionDomain": "",
      "signingIdentity": null,
      "providerShortName": null,
      "entitlements": null
    },
    "windows": {
      "certificateThumbprint": null,
      "digestAlgorithm": "sha256",
      "timestampUrl": ""
    }
  },
  "plugins": {
    "updater": {
      "active": true,
      "endpoints": [
        "https://releases.anureo.app/{{target}}/{{current_version}}"
      ],
      "dialog": true,
      "pubkey": "YOUR_PUBLIC_KEY_HERE"
    }
  },
  "tauri": {
    "allowlist": {
      "all": true,
      "shell": {
        "all": true,
        "open": true
      },
      "window": {
        "all": true,
        "close": true,
        "hide": true,
        "show": true,
        "maximize": true,
        "minimize": true,
        "unmaximize": true,
        "unminimize": true,
        "startDragging": true
      },
      "notification": {
        "all": true
      },
      "fs": {
        "all": true,
        "scope": ["**"]
      },
      "path": {
        "all": true
      },
      "dialog": {
        "all": true
      },
      "clipboard": {
        "all": true
      },
      "globalShortcut": {
        "all": true
      }
    }
  }
}
```

### 4.2 Cargo.toml

```toml
[package]
name = "anureo"
version = "1.0.0"
description = "Anureo - AI Agent Desktop Application"
authors = ["Anureo Team"]
license = "MIT"
repository = "https://github.com/your-org/anureo"
edition = "2021"

[build-dependencies]
tauri-build = { version = "2.0", features = [] }

[dependencies]
tauri = { version = "2.0", features = ["shell-open"] }
tauri-plugin-shell = "2.0"
tauri-plugin-notification = "2.0"
tauri-plugin-fs = "2.0"
tauri-plugin-dialog = "2.0"
tauri-plugin-clipboard-manager = "2.0"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
tokio = { version = "1.35", features = ["full"] }
tokio-tungstenite = "0.21"
futures-util = "0.3"
uuid = { version = "1.6", features = ["v4", "serde"] }
url = "2.5"
thiserror = "1.0"
anyhow = "1.0"

[features]
default = ["custom-protocol"]
custom-protocol = ["tauri/custom-protocol"]
```

## 5. 前端集成

### 5.1 Tauri API 封装

```typescript
// src-tauri/src/lib.ts 前端调用接口
import { invoke } from '@tauri-apps/api/tauri';
import { listen } from '@tauri-apps/api/event';

export interface WindowConfig {
  label: string;
  title: string;
  url: string;
}

export interface NotificationConfig {
  title: string;
  body: string;
  icon?: string;
}

export const tauriApi = {
  // 窗口管理
  window: {
    create: (config: WindowConfig) => 
      invoke('create_window', { 
        label: config.label, 
        title: config.title, 
        url: config.url 
      }),
    
    focus: () => invoke('focus_window'),
    minimize: () => invoke('minimize_window'),
    close: () => invoke('close_window'),
  },
  
  // 系统托盘
  tray: {
    show: () => invoke('show_tray'),
    hide: () => invoke('hide_tray'),
  },
  
  // 通知
  notification: {
    show: (config: NotificationConfig) => 
      invoke('show_notification', config),
    
    showAgentCompletion: (agentName: string, sessionId: string, result: string) =>
      invoke('show_agent_completion_notification', { 
        agentName, 
        sessionId, 
        result 
      }),
    
    showError: (errorMessage: string) =>
      invoke('show_error_notification', { errorMessage }),
  },
  
  // 文件系统
  fs: {
    openFile: () => invoke('open_file_dialog'),
    openFolder: () => invoke('open_folder_dialog'),
    saveFile: (content: string, defaultPath: string) => 
      invoke('save_file', { content, defaultPath }),
  },
  
  // 事件监听
  events: {
    onProjectOpen: (callback: (projectPath: string) => void) => 
      listen('open-project', (event) => callback(event.payload as string)),
    
    onAgentComplete: (callback: (data: any) => void) =>
      listen('agent-complete', (event) => callback(event.payload)),
    
    onError: (callback: (error: string) => void) =>
      listen('error', (event) => callback(event.payload as string)),
  }
};
```

### 5.2 现有代码适配

```typescript
// 在现有的 anureo 代码中集成 Tauri 功能
import { tauriApi } from '@tauri-apps/api/lib';

// 替换现有的通知调用
const showNotification = (title: string, body: string) => {
  if (window.__TAURI__) {
    // Tauri 环境
    tauriApi.notification.show({ title, body });
  } else {
    // 浏览器环境
    new Notification(title, { body });
  }
};

// 替换现有的文件打开
const openProject = async (projectPath: string) => {
  if (window.__TAURI__) {
    // Tauri 环境 - 可以直接访问文件系统
    await tauriApi.fs.openFolder();
  } else {
    // 浏览器环境 - 使用现有的文件选择器
    // 现有逻辑
  }
};

// 监听系统托盘事件
useEffect(() => {
  if (window.__TAURI__) {
    const unlisten = tauriApi.events.onProjectOpen((projectPath) => {
      // 处理项目打开
      openProject(projectPath);
    });
    
    return () => {
      unlisten.then(fn => fn());
    };
  }
}, []);
```

## 6. 构建和打包

### 6.1 开发环境

```bash
# 在 anureo-feat-dev 目录下

# 安装 Tauri CLI
npm install -g @tauri-apps/cli

# 初始化 Tauri（如果还没有）
npm run tauri init

# 开发模式
npm run tauri dev

# 这将：
# 1. 启动 Vite 开发服务器 (http://localhost:5180)
# 2. 编译 Tauri 后端
# 3. 启动 Tauri 应用窗口
# 4. 支持热重载
```

### 6.2 生产构建

```bash
# 构建所有平台
npm run tauri build

# 构建特定平台
npm run tauri build -- --target x86_64-pc-windows-msvc
npm run tauri build -- --target x86_64-apple-darwin
npm run tauri build -- --target x86_64-unknown-linux-gnu

# 构建产物位置：
# Windows: src-tauri/target/release/bundle/msi/
# macOS: src-tauri/target/release/bundle/dmg/
# Linux: src-tauri/target/release/bundle/appimage/
```

### 6.3 自动更新

```rust
// src-tauri/src/commands/updater.rs
use tauri_plugin_updater::UpdaterExt;

#[tauri::command]
async fn check_update(app_handle: tauri::AppHandle) -> Result<Option<String>, String> {
    if let Some(update) = app_handle.updater()?.check().await.map_err(|e| e.to_string())? {
        Ok(Some(update.version))
    } else {
        Ok(None)
    }
}

#[tauri::command]
async fn download_and_install_update(app_handle: tauri::AppHandle) -> Result<(), String> {
    if let Some(update) = app_handle.updater()?.check().await.map_err(|e| e.to_string())? {
        update.download_and_install().await.map_err(|e| e.to_string())?;
    }
    Ok(())
}
```

## 7. 实施计划

### Phase 1: 基础设施 (Week 1-2)

- [ ] 初始化 Tauri 项目结构
- [ ] 配置构建环境
- [ ] 实现基础窗口管理
- [ ] 集成现有前端代码

### Phase 2: 系统集成 (Week 3-4)

- [ ] 实现系统托盘功能
- [ ] 实现系统通知
- [ ] 实现文件关联
- [ ] 实现快捷键支持

### Phase 3: ACP 通信层 (Week 5-6)

- [ ] 实现 ACP WebSocket 客户端
- [ ] 实现重连机制
- [ ] 实现消息路由
- [ ] 错误处理和恢复

### Phase 4: 测试和优化 (Week 7-8)

- [ ] 功能测试
- [ ] 性能优化
- [ ] 跨平台测试
- [ ] 用户体验优化

### Phase 5: 打包和分发 (Week 9-10)

- [ ] 配置应用签名
- [ ] 实现自动更新
- [ ] 准备安装程序
- [ ] 文档和发布

## 8. 风险和挑战

### 8.1 技术风险

1. **ACP 协议兼容性**
   - 风险: WebSocket 连接在 Tauri 环境中的行为可能与浏览器不同
   - 缓解: 充分测试，实现完善的错误处理和重连机制

2. **跨平台兼容性**
   - 风险: 不同平台的系统 API 差异
   - 缓解: 使用 Tauri 的跨平台抽象，针对性测试

3. **性能问题**
   - 风险: 桌面应用可能比 Web 应用占用更多资源
   - 缓解: 性能监控，优化资源使用

### 8.2 用户体验风险

1. **学习曲线**
   - 风险: 用户需要适应新的桌面应用界面
   - 缓解: 保持界面与 Web 版本一致，提供平滑迁移

2. **功能对等**
   - 风险: 桌面版功能可能与 Web 版不一致
   - 缓解: 功能对等测试，逐步迁移

### 8.3 开发风险

1. **开发复杂度**
   - 风险: 需要维护两套代码（Web + Desktop）
   - 缓解: 最大化代码复用，共享业务逻辑

2. **调试难度**
   - 风险: 桌面应用调试比 Web 应用复杂
   - 缓解: 完善的日志系统，远程调试支持

## 9. 成功指标

### 9.1 技术指标

- ✅ 应用启动时间 < 3秒
- ✅ 内存占用 < 200MB
- ✅ ACP 连接成功率 > 99%
- ✅ 跨平台兼容性 (Windows, macOS, Linux)

### 9.2 用户体验指标

- ✅ 用户满意度 > 4.5/5
- ✅ 功能完整度 100% (与 Web 版对等)
- ✅ 系统集成功能正常工作率 > 95%

### 9.3 开发指标

- ✅ 代码复用率 > 80%
- ✅ 测试覆盖率 > 70%
- ✅ 构建时间 < 5分钟

## 10. 后续优化方向

### 10.1 短期优化

1. **性能优化**
   - 减少内存占用
   - 优化启动速度
   - 改善渲染性能

2. **用户体验**
   - 添加更多桌面特性
   - 优化界面响应
   - 改善错误提示

### 10.2 长期规划

1. **离线功能**
   - 支持离线模式
   - 本地数据缓存
   - 增量同步

2. **插件系统**
   - 支持第三方插件
   - 插件市场
   - API 扩展

3. **多实例管理**
   - 支持多个 anureo 实例
   - 实例切换
   - 资源隔离

这个方案提供了一个完整的 Tauri 集成路径，保持了现有架构的优势，同时提供了桌面应用的原生体验。实施时可以根据实际需求和资源情况调整优先级和时间安排。