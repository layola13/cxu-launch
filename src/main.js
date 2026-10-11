// 无构建前端：经 window.__TAURI__ 全局对象调用（需 tauri.conf withGlobalTauri=true）。
// 不用裸包导入 "@tauri-apps/api/..." —— file:// 下无 importmap 会解析失败，
// 导致整个 module 静默不执行（界面永远停在初始静态文案，这正是之前“一直检测中”的样子）。
const tauriApi = window.__TAURI__;
const $ = (id) => document.getElementById(id);
const logEl = $("log");
function log(line) {
  logEl.textContent += `\n${new Date().toLocaleTimeString()} ${line}`;
  logEl.scrollTop = logEl.scrollHeight;
}

if (!tauriApi || !tauriApi.core || !tauriApi.event) {
  $("subtitle").textContent = "前端启动失败：Tauri API 不可用。";
  log("致命错误: window.__TAURI__ 不可用，请确认安装包完整并截图反馈。");
  throw new Error("Tauri API unavailable");
}

const { invoke } = tauriApi.core;
const { listen } = tauriApi.event;

async function refresh() {
  try {
    const s = await invoke("launcher_status");
    $("localVer").textContent = s.cxu_path ? `${s.cxu_version ?? "?"} @ ${s.cxu_path}` : "未安装";
    $("remoteVer").textContent = s.latest_version ?? "—";
    $("serveUrl").textContent = s.url ?? "—";
    $("serveState").textContent = s.running
      ? s.pid
        ? `running (pid ${s.pid})`
        : `running (外部已启动, 端口 ${s.port})`
      : "stopped";
    $("subtitle").textContent = s.running
      ? s.pid
        ? "服务运行中，可打开工作台。"
        : "检测到已运行的服务，已自动复用，可打开工作台。"
      : s.cxu_path
        ? "已就绪，点击启动服务。"
        : "未检测到 cxu，请先安装。";
    $("dot").className = `dot ${s.running ? "ok" : s.cxu_path ? "busy" : "err"}`;
    $("btnStart").disabled = s.running || !s.cxu_path;
    $("btnOpen").disabled = !s.running;
    $("btnRestart").disabled = !s.running;
    $("btnInstall").disabled = !!s.cxu_path;
    $("btnUpdate").disabled = !s.cxu_path || !s.latest_version || s.cxu_version === s.latest_version;
    if (s.latest_version && s.cxu_version && s.cxu_version !== s.latest_version) {
      $("btnUpdate").textContent = `更新 cxu (${s.cxu_version} → ${s.latest_version})`;
    }
  } catch (e) {
    log(`状态查询失败: ${String(e)}`);
  }
}

async function run(fn, busyText) {
  // 操作期间锁住动作按钮：后端 launcher_start 最长跑 20s，没有反馈用户就会连点。
  if (window.__launcherBusy) {
    log("已有操作在进行，请稍候…");
    return;
  }
  window.__launcherBusy = true;
  for (const id of ["btnStart", "btnRestart", "btnInstall", "btnUpdate"]) {
    $(id).disabled = true;
  }
  try {
    $("subtitle").textContent = busyText;
    await fn();
  } catch (e) {
    log(`失败: ${String(e)}`);
  } finally {
    window.__launcherBusy = false;
    await refresh();
  }
}

$("btnStart").onclick = () => run(() => invoke("launcher_start"), "正在启动 cxu serve…");
$("btnRestart").onclick = () => run(() => invoke("launcher_restart"), "正在重启服务…");
$("btnInstall").onclick = () => run(() => invoke("launcher_install"), "正在安装 cxu（npm + doctor）…");
$("btnUpdate").onclick = () => run(() => invoke("launcher_update"), "正在更新 cxu…");
$("btnOpen").onclick = () => {
  invoke("launcher_open_ui")
    .then(() => log("已调用系统浏览器打开工作台。"))
    .catch((e) => log(`打开工作台失败: ${String(e)}`));
};
$("btnLogDir").onclick = () => invoke("launcher_open_log_dir");
$("btnHide").onclick = () => invoke("launcher_hide");
// 退出后进程直接结束，不再 refresh（否则只会刷出一行失败日志）。
$("btnQuit").onclick = () => {
  $("subtitle").textContent = "正在退出启动器…";
  invoke("launcher_quit").catch((e) => log(`退出失败: ${String(e)}`));
};

try {
  await listen("cxu-log", (e) => log(String(e.payload)));
} catch (e) {
  log(`日志通道订阅失败: ${String(e)}`);
}
// 上报存活：后端记一行“前端已加载”，证明 IPC 双向正常（排障用）。
try {
  await invoke("launcher_front_ready");
} catch (e) {
  log(`就绪上报失败: ${String(e)}`);
}
window.__launcherBooted = true;
// 防重入 + 先注册轮询：单次查询再慢也只跳过一拍，不会把 UI 拖死。
// 后端 status 已是纯 socket 探测（毫秒级），这里只是双保险。
let refreshing = false;
async function refreshGuarded() {
  if (refreshing) return;
  refreshing = true;
  try {
    await refresh();
  } finally {
    refreshing = false;
  }
}
setInterval(refreshGuarded, 3000);
await refreshGuarded();
