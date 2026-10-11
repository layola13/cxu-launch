# cxu-launch — Codex Ultra 极简桌面启动器

Tauri 2 + 系统 WebView2，无内嵌 Chromium、无内嵌后端。独立工程，不在 `codex-ultra` 主仓内。

只做三件事：

1. **桌面快捷方式**：NSIS 安装包 `createDesktopShortcut=true`（`src-tauri/tauri.conf.json`），另有开始菜单项。
2. **最简启动器**：检查 cxu 安装 → 更新 → 启动 `cxu serve --port <空闲口>`，健康检查 `GET /api/runtime/health`，就绪后“一键在系统浏览器打开工作台”。
3. **托盘**：关闭窗口仅隐藏；托盘菜单含 打开启动器 / 打开工作台 / 重启服务 / 退出（停服务）。

## 目录

```
cxu-launch/
  src/                 # 启动器小窗（无构建，原生 HTML/CSS/JS）
  src-tauri/
    src/main.rs        # cxu 进程管理 + 健康检查 + 托盘 + 单例
    tauri.conf.json    # identifier com.codexultra.launcher, targets nsis
    capabilities/      # 最小权限（event + window hide/show + opener）
  package.json
```

## 前置

- Windows 10 1803+（自带 WebView2；极老系统需手动装 WebView2 Runtime）
- Node.js 18+（`npm install -g @codex-ultra/cxu` 用）
- Rust stable（`rustup`），Tauri CLI 通过 npm 自带

## 运行

```powershell
cd D:\projects\cxu-launch
npm install
npm run dev
```

首次需图标资源（二进制构建必需）：

```powershell
# 把主仓图标拷过来生成全尺寸图标
Copy-Item D:\projects\codex-ultra\apps\web\public\icons\icon-512.png -Destination .\src-tauri\icons\icon.png -Force
npm run tauri icon src-tauri\icons\icon.png
```

## 打包（Windows NSIS）

```powershell
npm run build:nsis
# 产物：src-tauri\target\release\bundle\nsis\*.exe
```

## 打包（Linux / macOS，可交叉编译）

本工程已配置多目标 bundle（`src-tauri/tauri.conf.json` targets 含 `nsis,deb,rpm,appimage,dmg,app`），
标准做法是各 OS 原生编译（CI 矩阵），见 `.github/workflows/release.yml`：

```bash
npm ci
# Linux 本机：deb + appimage
npm run build:linux
# 产物：src-tauri/target/release/bundle/deb/*.deb
#      src-tauri/target/release/bundle/appimage/*.AppImage

# macOS 本机：dmg + app
npm run build:mac
# 产物：src-tauri/target/release/bundle/dmg/*.dmg
#      src-tauri/target/release/bundle/macos/*.app
```

Linux 前置：`libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev` 等；
macOS 要求 10.15+，Windows 仍用 NSIS。

Release 流程：打 tag `v*` 即触发 6 任务矩阵构建（x64 + ARM 三平台同一 Release）：

```bash
git tag v0.2.0 && git push origin v0.2.0
```

| # | Runner | Target | 产物 |
|---|--------|--------|------|
| win x64 | windows-latest | 原生 | NSIS `*_x64-setup.exe` |
| win ARM64 | windows-latest | `aarch64-pc-windows-msvc` 交叉 | NSIS `*_arm64-setup.exe` |
| linux x64 | ubuntu-22.04 | 原生 | `*.deb` + `*.AppImage` |
| linux ARM64 | ubuntu-24.04-arm | 原生 ARM runner | `*.deb` + `*.AppImage` |
| mac ARM64 | macos-latest | `aarch64-apple-darwin` | `*_aarch64.dmg` + `.app` |
| mac Intel | macos-latest | `x86_64-apple-darwin` 交叉 | `*_x64.dmg` + `.app` |

## 行为契约（与老 Electron 壳对齐）

- 端口：`43110` 起顺延 20 个，`127.0.0.1`，与 `desktop/main.cjs chooseDesktopPort` 一致。
- 健康：`GET http://127.0.0.1:<port>/api/runtime/health`，2xx–4xx 即就绪，20s 超时。
- 安装链：`npm install -g @codex-ultra/cxu && cxu doctor --yes`（与 `CXU_SETUP_CHAIN` 同语义）。
- 日志：Windows `%APPDATA%\cxu-launch\logs\launcher.log` + `cxu-serve.log`；Linux `$XDG_DATA_HOME/cxu-launch/logs`（fallback `~/.local/share/cxu-launch/logs`）；macOS `~/Library/Logs/cxu-launch`。
- 单例：第二个实例自动唤起已运行窗口，不另起服务。
- 退出语义：关窗=隐藏；托盘“退出”才 kill `cxu serve` 子进程。

## 与老版本共存

- identifier 用 `com.codexultra.launcher`（老壳是 `com.codexultra.desktop`），可并存验证。
- 若要替换老壳升级，需把 identifier 改回 `com.codexultra.desktop` 并沿用 NSIS GUID（见主仓 `electron-builder.yml`），此处默认不抢占。
