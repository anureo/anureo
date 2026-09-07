// Unified PM2 configuration for anureo development environments.
// Set ANUREO_PM2_PROFILE to dev (default) or local before start.

const fs = require("fs");
const path = require("path");

const root = path.resolve(__dirname, "..");
const frontend = process.env.ANUREO_FRONTEND_ROOT
  ? path.resolve(process.env.ANUREO_FRONTEND_ROOT)
  : path.resolve(root, "..", "openchamber-feat-dev");
const userHome = process.env.USERPROFILE || process.env.HOME || "";
const bun = path.join(userHome, ".bun", "bin", "bun.exe");
const localHome = path.join(root, ".anureo-home-local");
const localUiPasswordPath = path.join(localHome, "ui-password.txt");
const localUiPassword = fs.existsSync(localUiPasswordPath)
  ? fs.readFileSync(localUiPasswordPath, "utf8").trim()
  : undefined;
const common = { time: true, autorestart: true, watch: false };

const profiles = {
  dev: [
    {
      ...common,
      name: "anureo-dev",
      script: path.join(root, "target", "debug", "anureo.exe"),
      args: "server --port 3031 --home .anureo-home --pid-file .anureo-home/anureo-server.pid --log-level trace --log-file .anureo-home/anureo-dev.log",
      cwd: root,
      env: { ANUREO_DATA_DIR: path.join(root, ".anureo-home") },
      max_restarts: 10,
      min_uptime: "10s",
    },
    {
      ...common,
      name: "anureo-desk",
      script: bun,
      args: "run dev",
      cwd: frontend,
      env: { ANUREO_ACP_BASE_URL: "http://127.0.0.1:3031" },
    },
  ],
  local: [
    {
      ...common,
      name: "anureo-local",
      script: path.join(root, "target", "debug", "anureo.exe"),
      args: "server --port 3051 --home .anureo-home-local --pid-file .anureo-home-local/anureo-server.pid",
      cwd: root,
      env: {
        ANUREO_ACP_ALLOWED_ORIGINS:
          "https://loom.getxagent.com,http://localhost:3151,http://127.0.0.1:3151",
        ANUREO_DATA_DIR: localHome,
        ANUREO_UI_PASSWORD: localUiPassword,
        LOOMDESK_DATA_DIR: path.join(root, ".anureo-home-local", "loomdesk-data"),
      },
      max_memory_restart: "1G",
    },
    {
      ...common,
      name: "anureo-desk-local",
      script: process.execPath,
      args: "packages/web/server/index.js --port 3151",
      cwd: frontend,
      env: {
        PORT: "3151",
        NODE_ENV: "production",
        OPENCODE_SKIP_START: "true",
        ANUREO_ACP_BASE_URL: "http://127.0.0.1:3051",
        ANUREO_DATA_DIR: localHome,
        ANUREO_UI_PASSWORD: localUiPassword,
        LOOMDESK_DATA_DIR: path.join(root, ".anureo-home-local", "loomdesk-data"),
      },
      max_memory_restart: "1G",
    },
  ],
};

const profile = process.env.ANUREO_PM2_PROFILE || "dev";
if (!profiles[profile]) {
  throw new Error(`Unknown ANUREO_PM2_PROFILE=${profile}; expected dev or local`);
}

module.exports = { apps: profiles[profile] };
