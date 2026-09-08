# PM2 dev environment manager

PM2 管理 `anureo-server`、`anureo-api`、`anureo-web` 三个开发进程。

## 文件

- `run-anureo.cjs` — Node.js 包装器,spawn `cargo run -p anureo-server -- serve --host 127.0.0.1 --port 18081`
- `run-anureo-api.cjs` — spawn `bun --cwd packages/web server/index.js --port 3902`,设置 `OPENCODE_HOST=http://127.0.0.1:18081` 和 `OPENCODE_SKIP_START=true`
- `run-anureo-web.cjs` — spawn `bun x vite --force --host 127.0.0.1 --port 5180 --strictPort`,在 `packages/web` 目录下
- `ecosystem.config.cjs` — PM2 配置

## 启动 / 停止

```powershell
pm2 start C:\Users\heycj\dev\worktrees\anureo\cli-server-backend\scripts\pm2\ecosystem.config.cjs
pm2 status
pm2 stop all
pm2 restart all
pm2 delete all

pm2 restart anureo-server
pm2 restart anureo-api
pm2 restart anureo-web

pm2 logs              # 所有
pm2 logs anureo-server  # 单个
pm2 flush             # 清日志
```

## 端口链路

```
浏览器 (5180)  →  vite (UI + 反向代理)
                   ├─ /health  ──→  anureo-api (3902)  →  anureo-server (18081)
                   ├─ /auth    ──→  anureo-api (3902)  →  anureo-server (18081)
                   └─ /api/*   ──→  anureo-api (3902)  →  anureo-server (18081)
```

| 服务 | 端口 | 验证 |
|------|------|------|
| anureo-server | 18081 | `http://127.0.0.1:18081/global/health` |
| anureo-ui (vite) | 5180 | `http://127.0.0.1:5180` 返回 200 |
| anureo-api | 3902 | `http://127.0.0.1:3902/health` 返回 200,`openCodePort=18081` |
| vite proxy → api | 5180/3902 | `http://127.0.0.1:5180/api/session` 返回 200 |

## 踩过的坑 (重要!)

### 1. PM2 on Windows 默认 interpreter 是 `node.exe`
直接 `script: '*.cmd'` 会 `SyntaxError: Invalid or unexpected token`。
修复: 用 `.cjs` 包装器,PM2 跑 Node.js,Node.js spawn 子进程。

### 2. `dev:web:hmr` 的 `dev:server:watch` 用 bash 语法
包脚本 `"dev:server:watch": "nodemon ... --exec \"bun server/index.js --port ${ANUREO_PORT:-3001}\""` 在 Windows cmd.exe 下 `${VAR:-default}` 不展开,bun 收到字面字符串。
修复: 跳过 nodemon,直接 spawn `bun server/index.js --port 3902`。

### 3. vite 不认识 `--cwd`
`bun x vite --cwd packages/web` 会 `CACError: Unknown option --cwd`。
修复: Node.js spawn 用 `cwd` 选项设工作目录,命令行只传 vite 认识的参数。

### 4. 孤儿进程占端口 (`AddrInUse`)
PM2 重启时如果旧 cmd.exe 已被 kill 但 cargo/bun 子进程还在,新进程 bind 失败 → exit 1 → 疯狂重启。
修复: 启动前手动清理:
```powershell
Get-Process anureo-server, cargo, rustc, bun, nodemon | Stop-Process -Force
```

### 5. PM2 缓存旧 config
`pm2 delete` 不会清掉 daemon 缓存的 `pm_exec_path` 和 `exec_interpreter`。即使你改了 `ecosystem.config.cjs`,新 `pm2 start` 可能还是用旧的 .cmd。
修复: `pm2 kill` 杀 daemon,清 `~/.pm2/pm2.log`,再 start。

## 端口清理脚本 (出问题就跑一遍)

```powershell
pm2 kill
Get-Process anureo-server, cargo, rustc, bun, nodemon -ErrorAction SilentlyContinue | Stop-Process -Force
Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue | Where-Object { $_.LocalPort -in 18081,5180,3902 } | ForEach-Object { Stop-Process -Id $_.OwningProcess -Force }
Remove-Item C:\Users\heycj\.pm2\pm2.log -ErrorAction SilentlyContinue
pm2 start C:\Users\heycj\dev\worktrees\anureo\cli-server-backend\scripts\pm2\ecosystem.config.cjs
```

## 日志位置

- `logs/pm2/anureo-out.log` / `anureo-error.log`
- `logs/pm2/anureo-api-out.log` / `anureo-api-error.log`
- `logs/pm2/anureo-web-out.log` / `anureo-web-error.log`