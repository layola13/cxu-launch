// cxu-launch: minimal Tauri 2 launcher. No embedded browser, no embedded backend.
// Manages a `cxu serve` child process, opens the workbench in the system browser.
#![cfg_attr(windows, windows_subsystem = "windows")]

use serde::Serialize;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State};

const PREFERRED_PORT: u16 = 43110;
const HOST: &str = "127.0.0.1";

struct LauncherState {
  child: Option<Child>,
  port: u16,
  starting: bool,
}

impl Default for LauncherState {
  fn default() -> Self {
    Self { child: None, port: PREFERRED_PORT, starting: false }
  }
}

// 服务状态快照：由后台监控线程每 ~2s 刷新一次。
// launcher_status / launcher_open_ui 只读快照、零 I/O —— 永远不会卡死。
struct ServiceCache {
  inner: Mutex<(bool, Option<u32>, u16)>,
}

impl Default for ServiceCache {
  fn default() -> Self {
    Self { inner: Mutex::new((false, None, PREFERRED_PORT)) }
  }
}

// Windows 下拉起控制台子进程默认会闪一个黑框（where/npm/cxu.cmd 都是控制台程序）。
// GUI 启动器必须全部 CREATE_NO_WINDOW，否则前端每 3s 一次 status 就会闪一次黑框。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

fn silent_cmd(program: &str) -> Command {
  #[allow(unused_mut)]
  let mut cmd = Command::new(program);
  #[cfg(windows)]
  {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
  }
  cmd
}

// 本机 cxu 定位缓存：find_cxu/cxu_version 都要拉子进程（慢 + 闪黑框），
// 绝不能放在每 3s 一次的 launcher_status 热路径里。后台线程慢刷，status 只读缓存。
struct CxuCache {
  inner: Mutex<(Option<String>, Option<String>)>,
}

impl Default for CxuCache {
  fn default() -> Self {
    Self { inner: Mutex::new((None, None)) }
  }
}

fn refresh_cxu_cache(app: &AppHandle) {
  let path = find_cxu();
  let ver = path.as_deref().and_then(cxu_version);
  let cache: State<'_, CxuCache> = app.state();
  *cache.inner.lock().unwrap() = (path, ver);
}

#[derive(Serialize, Clone)]
struct LauncherStatus {
  cxu_path: Option<String>,
  cxu_version: Option<String>,
  latest_version: Option<String>,
  running: bool,
  pid: Option<u32>,
  port: u16,
  url: Option<String>,
}

// npm 远端版本缓存：launcher_status 必须是非阻塞的（前端每 3s 轮询一次），
// 不能在 status 里同步执行 `npm view`（网络波动时会卡住整个小窗，表现为一直“检测中…”）。
struct LatestCache {
  inner: Mutex<(Option<String>, Option<std::time::Instant>)>,
}

impl Default for LatestCache {
  fn default() -> Self {
    Self { inner: Mutex::new((None, None)) }
  }
}

fn cached_latest(cache: &LatestCache) -> Option<String> {
  cache.inner.lock().unwrap().0.clone()
}

fn store_latest(cache: &LatestCache, v: Option<String>) {
  let mut g = cache.inner.lock().unwrap();
  g.0 = v;
  g.1 = Some(std::time::Instant::now());
}

fn fetch_latest_blocking() -> Option<String> {
  // 后台线程执行，hang 住也不影响 UI；最多 10 分钟刷一次。
  let out = silent_cmd("npm").args(["view", "@codex-ultra/cxu", "version"]).output().ok()?;
  if !out.status.success() {
    return None;
  }
  let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
  if v.is_empty() { None } else { Some(v) }
}

fn log_dir() -> PathBuf {
  if cfg!(windows) {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(base).join("cxu-launch").join("logs")
  } else if cfg!(target_os = "macos") {
    // macOS: ~/Library/Logs/cxu-launch (与系统日志约定一致)
    if let Some(home) = home_dir_fallback() {
      return home.join("Library").join("Logs").join("cxu-launch");
    }
    PathBuf::from(".").join("logs")
  } else {
    // Linux: $XDG_DATA_HOME/cxu-launch/logs, fallback ~/.local/share/cxu-launch/logs
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
      if !xdg.is_empty() {
        return PathBuf::from(xdg).join("cxu-launch").join("logs");
      }
    }
    if let Some(home) = home_dir_fallback() {
      return home.join(".local").join("share").join("cxu-launch").join("logs");
    }
    PathBuf::from(".").join("logs")
  }
}

fn emit_log(app: &AppHandle, line: &str) {
  let _ = app.emit("cxu-log", line.to_string());
  try_append_log(line);
}

fn try_append_log(line: &str) {
  let dir = log_dir();
  if std::fs::create_dir_all(&dir).is_err() {
    return;
  }
  let path = dir.join("launcher.log");
  // 日志轮转：超过 512KB 就只保留最后约 128KB，避免常驻后台把文件养到上百 MB。
  if let Ok(meta) = std::fs::metadata(&path) {
    if meta.len() > 512 * 1024 {
      if let Ok(text) = std::fs::read(&path) {
        let keep = text.len().saturating_sub(128 * 1024);
        let start = text[keep..].iter().position(|&b| b == b'\n').map(|i| keep + i + 1).unwrap_or(keep);
        let _ = std::fs::write(&path, &text[start..]);
      }
    }
  }
  let mut f = match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
    Ok(f) => f,
    Err(_) => return,
  };
  let _ = writeln!(f, "{:?} {line}", std::time::SystemTime::now());
}

// ---- cxu discovery (mirrors desktop/main.cjs findCxu) ----
fn home_dir_fallback() -> Option<PathBuf> {
  std::env::var("USERPROFILE").map(PathBuf::from).ok().or_else(|| {
    std::env::var("HOME").map(PathBuf::from).ok()
  })
}

fn find_cxu() -> Option<String> {
  #[cfg(windows)]
  {
    if let Ok(out) = silent_cmd("where.exe").args(["cxu", "cxu.cmd"]).output() {
      let text = String::from_utf8_lossy(&out.stdout);
      for line in text.lines() {
        let c = line.trim();
        if !c.is_empty() && std::path::Path::new(c).exists() {
          return Some(c.to_string());
        }
      }
    }
    if let Some(home) = home_dir_fallback() {
      for name in ["cxu.cmd", "cxu.exe", "cxu"] {
        let c = home.join(".cxu").join("bin").join(name);
        if c.exists() {
          return Some(c.to_string_lossy().to_string());
        }
      }
    }
    return None;
  }
  #[cfg(not(windows))]
  {
    if let Ok(out) = silent_cmd("which").arg("cxu").output() {
      let c = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_string();
      if !c.is_empty() && std::path::Path::new(&c).exists() {
        return Some(c);
      }
    }
    None
  }
}

fn cxu_version(path: &str) -> Option<String> {
  let out = silent_cmd(path).arg("--version").output().ok()?;
  if !out.status.success() {
    return None;
  }
  let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
  if text.is_empty() {
    None
  } else {
    // 只剥掉 "cxu v" / 首字母 v 前缀。旧代码 .replace('v',"") 会删掉版本号里所有的 v。
    let t = text.strip_prefix("cxu v").or_else(|| text.strip_prefix("cxu ")).unwrap_or(&text);
    let t = t.strip_prefix('v').unwrap_or(t);
    let t = t.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
  }
}

// 健康探测超时：必须小。旧值 connect 800ms + read 1500ms，21 个端口串行扫一遍
// 无服务时要十几秒 —— 这就是启动/轮询卡死的直接原因。
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_millis(150);
const TCP_READ_TIMEOUT: Duration = Duration::from_millis(350);

fn tcp_open(port: u16) -> bool {
  let addr: std::net::SocketAddr = match format!("{HOST}:{port}").parse() {
    Ok(a) => a,
    Err(_) => return false,
  };
  std::net::TcpStream::connect_timeout(&addr, TCP_CONNECT_TIMEOUT).is_ok()
}

fn pick_port() -> u16 {
  for p in PREFERRED_PORT..=(PREFERRED_PORT + 20) {
    if std::net::TcpListener::bind((HOST, p)).is_ok() {
      return p;
    }
  }
  PREFERRED_PORT
}

fn health_ok(port: u16) -> bool {
  let addr = format!("{HOST}:{port}");
  let parsed: std::net::SocketAddr = match addr.parse() {
    Ok(a) => a,
    Err(_) => return false,
  };
  let Ok(mut s) = TcpStream::connect_timeout(&parsed, TCP_CONNECT_TIMEOUT) else {
    return false;
  };
  let _ = s.set_read_timeout(Some(TCP_READ_TIMEOUT));
  let req = "GET /api/runtime/health HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
  if s.write_all(req.as_bytes()).is_err() {
    return false;
  }
  // 状态行一定在前几百字节内；最多读几次、读到含 \r\n 就判，避免一次短读误判。
  let mut head = Vec::with_capacity(1024);
  let mut buf = [0u8; 512];
  for _ in 0..4 {
    match s.read(&mut buf) {
      Ok(0) => break,
      Ok(n) => {
        head.extend_from_slice(&buf[..n]);
        if head.windows(2).any(|w| w == b"\r\n") {
          break;
        }
        if head.len() >= 1024 {
          break;
        }
      }
      Err(_) => break,
    }
  }
  if head.is_empty() {
    return false;
  }
  let text = String::from_utf8_lossy(&head);
  text.contains("200") || text.contains("404") || text.contains("401")
}

fn child_alive(state: &Mutex<LauncherState>) -> (bool, Option<u32>) {
  let mut guard = state.lock().unwrap();
  if let Some(child) = guard.child.as_mut() {
    match child.try_wait() {
      Ok(None) => return (true, Some(child.id())),
      _ => {
        guard.child = None;
      }
    }
  }
  (false, None)
}

// 扫描 43110 起 21 个端口，复用任何已在运行的健康服务（含外部手动启动 / 上次残留进程）。
// 两段式：先 150ms 快探端口是否 open，只有 open 的才发 HTTP —— 关闭端口不再吃 800ms+。
fn probe_healthy_port() -> Option<u16> {
  for p in PREFERRED_PORT..=(PREFERRED_PORT + 20) {
    if !tcp_open(p) {
      continue;
    }
    if health_ok(p) {
      return Some(p);
    }
  }
  None
}

fn current_service(state: &Mutex<LauncherState>) -> (bool, Option<u32>, u16) {
  // 快路径：上次端口还健康就直接认（一次 TCP，毫秒级），避免每次全量扫 21 个端口。
  let port = state.lock().unwrap().port;
  if health_ok(port) {
    let (_, pid) = child_alive(state);
    return (true, pid, port);
  }
  // 慢路径：上次端口挂了。自己的子进程还在就认它（启动中，健康还没就绪）；
  // 否则全量扫一次找外部复用目标。
  {
    let (own_running, own_pid) = child_alive(state);
    if own_running {
      return (true, own_pid, port);
    }
  }
  if let Some(p) = probe_healthy_port() {
    state.lock().unwrap().port = p;
    return (true, None, p);
  }
  (false, None, port)
}

// Windows 上直接 spawn *.cmd / 无扩展名脚本会失败（CreateProcess 跑不了批处理），
// 之前 find_cxu 返回的第一个结果经常是这类路径 —— 点击启动就静默失败。
// 统一走这个 helper：脚本类包一层 cmd /C，可执行文件直接起，且一律无黑框。
fn spawn_cxu_serve(path: &str, port: u16, log_file: std::fs::File, err_file: std::fs::File) -> std::io::Result<Child> {
  let lower = path.to_lowercase();
  let is_script = lower.ends_with(".cmd") || lower.ends_with(".bat") || lower.ends_with(".ps1");
  let has_exe_ext = lower.ends_with(".exe");
  let needs_shell = is_script || !has_exe_ext;
  if needs_shell {
    let mut cmd = silent_cmd("cmd");
    cmd.args(["/C", path, "serve", "--port", &port.to_string()]);
    cmd.stdout(Stdio::from(log_file)).stderr(Stdio::from(err_file)).stdin(Stdio::null());
    cmd.spawn()
  } else {
    let mut cmd = silent_cmd(path);
    cmd.args(["serve", "--port", &port.to_string()]);
    cmd.stdout(Stdio::from(log_file)).stderr(Stdio::from(err_file)).stdin(Stdio::null());
    cmd.spawn()
  }
}

#[tauri::command]
fn launcher_status(
  cache: State<'_, LatestCache>,
  cxu: State<'_, CxuCache>,
  svc: State<'_, ServiceCache>,
) -> LauncherStatus {
  // 热路径：只读缓存、零 I/O —— 永远不会卡死。
  // 服务快照由后台监控线程每 ~2s 刷新；cxu 定位/版本由后台慢刷。
  let (path, ver) = cxu.inner.lock().unwrap().clone();
  let (running, pid, port) = svc.inner.lock().unwrap().clone();
  LauncherStatus {
    cxu_path: path,
    cxu_version: ver,
    latest_version: cached_latest(&cache),
    running,
    pid,
    port,
    url: running.then(|| format!("http://{HOST}:{port}")),
  }
}

#[tauri::command]
async fn launcher_start(
  app: AppHandle,
  state: State<'_, Mutex<LauncherState>>,
  cxu: State<'_, CxuCache>,
  svc: State<'_, ServiceCache>,
) -> Result<String, String> {
  // 防重入：上一次启动还在跑时直接拒绝，避免并发 probe 叠加把机器拖死。
  {
    let mut guard = state.lock().unwrap();
    if guard.starting {
      return Err("正在启动中，请稍候…".into());
    }
    if let Some(child) = guard.child.as_mut() {
      if child.try_wait().map(|s| s.is_none()).unwrap_or(false) {
        return Err("服务已在运行。".into());
      } else {
        guard.child = None;
      }
    }
    guard.starting = true;
  }
  let done = |state: &State<'_, Mutex<LauncherState>>| {
    state.lock().unwrap().starting = false;
  };
  // cxu 路径优先读缓存（后台已刷好，无子进程、无黑框）；缓存空才现找。
  let path: String = {
    let (cached, _) = cxu.inner.lock().unwrap().clone();
    match cached {
      Some(p) => p,
      None => find_cxu().ok_or("未找到 cxu，请先安装。")?,
    }
  };
  // 快路径：快照里已有健康服务（外部手动启动 / 上次残留），直接复用，立即返回。
  {
    let (running, _, port) = svc.inner.lock().unwrap().clone();
    if running && health_ok(port) {
      state.lock().unwrap().port = port;
      done(&state);
      let url = format!("http://{HOST}:{port}");
      emit_log(&app, &format!("检测到已在运行的服务，直接复用 {url}"));
      return Ok(url);
    }
  }
  let port = pick_port();
  state.lock().unwrap().port = port;
  emit_log(&app, &format!("启动 cxu: {path} serve --port {port}"));
  let _ = std::fs::create_dir_all(log_dir());
  let log_path = log_dir().join("cxu-serve.log");
  // 端口竞态重试：pick_port 选中到 serve 实际监听之间端口可能被抢，
  // 旧代码直接报“进程已退出”。改为最多顺延试 3 个端口。
  let mut last_err = String::new();
  for attempt in 0..3 {
    let try_port = port + attempt;
    if attempt > 0 {
      // 顺延的端口必须当前可 bind，否则跳过。
      if std::net::TcpListener::bind((HOST, try_port)).is_err() {
        continue;
      }
      state.lock().unwrap().port = try_port;
      emit_log(&app, &format!("端口 {port} 被占用，改试 {try_port}"));
    }
    let log_file = match std::fs::File::create(&log_path) {
      Ok(f) => f,
      Err(e) => {
        done(&state);
        return Err(e.to_string());
      }
    };
    let err_file = match log_file.try_clone() {
      Ok(f) => f,
      Err(e) => {
        done(&state);
        return Err(e.to_string());
      }
    };
    let child = match spawn_cxu_serve(&path, try_port, log_file, err_file) {
      Ok(c) => c,
      Err(e) => {
        last_err = format!("启动失败（spawn {path} 出错）: {e}，请看 cxu-serve.log。");
        continue;
      }
    };
    state.lock().unwrap().child = Some(child);
    // 等待就绪：每 500ms 探一次，共 20s；每 2s 打一行进度，前端轮询能看到活着。
    let mut ready = false;
    for i in 0..40 {
      tokio::time::sleep(Duration::from_millis(500)).await;
      let (alive, _) = child_alive(&state);
      if !alive {
        last_err = "cxu 进程已退出（端口可能被抢占），请看 cxu-serve.log。".to_string();
        break;
      }
      if health_ok(try_port) {
        ready = true;
        break;
      }
      if i % 4 == 3 {
        emit_log(&app, &format!("等待服务就绪…（{}s）", (i + 1) / 2));
      }
    }
    if ready {
      svc.inner.lock().unwrap().clone_from(&(true, child_alive_pid(&state), try_port));
      state.lock().unwrap().port = try_port;
      done(&state);
      let url = format!("http://{HOST}:{try_port}");
      emit_log(&app, &format!("服务就绪 {url}"));
      return Ok(url);
    }
    // 本轮没就绪（进程已退出）：继续试下一个端口；进程还在但 20s 未就绪则直接超时。
    {
      let guard = state.lock().unwrap();
      if guard.child.is_some() {
        drop(guard);
        done(&state);
        return Err("服务启动超时（20s），请看 cxu-serve.log。".into());
      }
    }
  }
  done(&state);
  Err(if last_err.is_empty() {
    "服务启动失败，请看 cxu-serve.log。".to_string()
  } else {
    last_err
  })
}

// 只读 pid，不收割 child（status 快照/日志用）。
fn child_alive_pid(state: &Mutex<LauncherState>) -> Option<u32> {
  let mut guard = state.lock().unwrap();
  if let Some(child) = guard.child.as_mut() {
    match child.try_wait() {
      Ok(None) => return Some(child.id()),
      _ => {
        guard.child = None;
      }
    }
  }
  None
}

#[tauri::command]
fn launcher_stop(state: State<'_, Mutex<LauncherState>>, svc: State<'_, ServiceCache>) -> Result<(), String> {
  {
    let mut guard = state.lock().unwrap();
    if let Some(mut child) = guard.child.take() {
      let _ = child.kill();
      let _ = child.wait();
    }
    guard.starting = false;
  }
  // 杀完立即按真实情况重算一次快照（而不是硬写 stopped）：
  // 无外部服务 → stopped；有外部服务 → 保持 running，不闪烁。
  let snap = {
    let st: &Mutex<LauncherState> = &state;
    current_service(st)
  };
  *svc.inner.lock().unwrap() = snap;
  Ok(())
}

#[tauri::command]
async fn launcher_restart(
  app: AppHandle,
  state: State<'_, Mutex<LauncherState>>,
  cxu: State<'_, CxuCache>,
  svc: State<'_, ServiceCache>,
) -> Result<String, String> {
  {
    let mut guard = state.lock().unwrap();
    if guard.starting {
      return Err("正在启动中，请稍候…".into());
    }
    if let Some(mut child) = guard.child.take() {
      let _ = child.kill();
      let _ = child.wait();
    } else {
      // 没有本启动器拉起的子进程：如果快照里有外部服务在跑，诚实拒绝，
      // 别演“重启成功”——启动器杀不掉外部进程，重启只会空转复用回来。
      let (running, _, port) = svc.inner.lock().unwrap().clone();
      if running && health_ok(port) {
        return Err(format!(
          "当前运行的是外部 cxu 服务（端口 {port}），启动器无法重启它：请先停掉外部服务，或直接用它。"
        ));
      }
    }
  }
  tokio::time::sleep(Duration::from_millis(500)).await;
  launcher_start(app, state, cxu, svc).await
}

fn run_blocking(app: &AppHandle, program: &str, args: &[&str], label: &str) -> Result<(), String> {
  emit_log(app, &format!("执行: {program} {}", args.join(" ")));
  let out = silent_cmd(program).args(args).output().map_err(|e| format!("{label}失败: {e}"))?;
  let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
  for line in text.lines().take(40) {
    emit_log(app, line);
  }
  if out.status.success() { Ok(()) } else { Err(format!("{label}失败，退出码 {:?}", out.status.code())) }
}

#[tauri::command]
async fn launcher_install(app: AppHandle) -> Result<(), String> {
  let app2 = app.clone();
  tauri::async_runtime::spawn_blocking(move || {
    run_blocking(&app2, "npm", &["install", "-g", "@codex-ultra/cxu"], "安装")?;
    let cxu = find_cxu().ok_or("安装后仍找不到 cxu，请检查 npm 全局 bin 是否在 PATH。")?;
    run_blocking(&app2, &cxu, &["doctor", "--yes"], "doctor")?;
    // 安装/更新后立即刷新缓存，前端下一次 3s 轮询就能看到，不用等后台 60s 节拍。
    let ver = cxu_version(&cxu);
    let cache: State<'_, CxuCache> = app2.state();
    *cache.inner.lock().unwrap() = (Some(cxu), ver);
    Ok(())
  })
  .await
  .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn launcher_update(app: AppHandle) -> Result<(), String> {
  launcher_install(app).await
}

#[tauri::command]
fn launcher_open_ui(app: AppHandle, svc: State<'_, ServiceCache>) -> Result<(), String> {
  // 点击即返回：只读服务快照，绝不在点击路径上做端口扫描；
  // 浏览器拉起丢独立线程 fire-and-forget，任何情况下都不卡 IPC、不卡窗口。
  let (running, _, port) = svc.inner.lock().unwrap().clone();
  if !running {
    return Err("服务未运行，请先点击“启动服务”。".into());
  }
  let url = format!("http://{HOST}:{port}");
  emit_log(&app, &format!("正在系统浏览器打开工作台 {url}"));
  std::thread::spawn(move || {
    if let Err(e) = tauri_plugin_opener::open_url(url, None::<&str>) {
      try_append_log(&format!("打开浏览器失败: {e}"));
    }
  });
  Ok(())
}

#[tauri::command]
fn launcher_open_log_dir() -> Result<(), String> {
  let dir = log_dir();
  let _ = std::fs::create_dir_all(&dir);
  #[cfg(windows)]
  {
    Command::new("explorer.exe").arg(&dir).spawn().map_err(|e| e.to_string())?;
    return Ok(());
  }
  #[cfg(target_os = "macos")]
  {
    Command::new("open").arg(&dir).spawn().map_err(|e| e.to_string())?;
    return Ok(());
  }
  #[cfg(not(any(windows, target_os = "macos")))]
  {
    Command::new("xdg-open").arg(&dir).spawn().map_err(|e| e.to_string())?;
    return Ok(());
  }
}

#[tauri::command]
async fn launcher_hide(app: AppHandle) -> Result<(), String> {
  if let Some(w) = app.get_webview_window("main") {
    let _ = w.hide();
  }
  Ok(())
}

// 前端存活探针：页面 JS 成功加载后调用一次。既证明 IPC 通道正常，
// 也给运行日志留一行，便于排查“界面卡死到底是前端还是后端问题”。
#[tauri::command]
fn launcher_front_ready(app: AppHandle) -> Result<(), String> {
  emit_log(&app, "前端已加载，状态通道正常。");
  Ok(())
}

// 可见的退出入口：停掉自己拉起的子进程后退出整个应用。
// 注意：复用来的外部服务（非本启动器拉起）不会被杀，只退出启动器自身。
#[tauri::command]
fn launcher_quit(app: AppHandle, state: State<'_, Mutex<LauncherState>>) -> Result<(), String> {
  let had_own_child = {
    let mut guard = state.lock().unwrap();
    if let Some(mut child) = guard.child.take() {
      let _ = child.kill();
      let _ = child.wait();
      true
    } else {
      false
    }
  };
  if had_own_child {
    emit_log(&app, "已停止本启动器拉起的服务，正在退出。");
  } else {
    emit_log(&app, "没有本启动器拉起的服务（外部服务不受影响），正在退出。");
  }
  // 让日志有机会落盘再走。
  std::thread::sleep(Duration::from_millis(300));
  app.exit(0);
  #[allow(unreachable_code)]
  Ok(())
}

fn show_main(app: &AppHandle) {
  if let Some(w) = app.get_webview_window("main") {
    let _ = w.show();
    let _ = w.set_focus();
  }
}

fn main() {
  let state = Mutex::new(LauncherState::default());
  let latest = LatestCache::default();
  let cxu = CxuCache::default();
  let svc = ServiceCache::default();
  tauri::Builder::default()
    .manage(state)
    .manage(latest)
    .manage(cxu)
    .manage(svc)
    .plugin(tauri_plugin_opener::init())
    .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
      show_main(app);
    }))
    .invoke_handler(tauri::generate_handler![
      launcher_status,
      launcher_start,
      launcher_restart,
      launcher_stop,
      launcher_install,
      launcher_update,
      launcher_open_ui,
      launcher_open_log_dir,
      launcher_hide,
      launcher_front_ready,
      launcher_quit
    ])
    .setup(|app| {
      let handle = app.handle().clone();
      let open = MenuItem::with_id(app, "open", "打开 cxu 启动器", true, None::<&str>)?;
      let open_ui = MenuItem::with_id(app, "open_ui", "在浏览器打开工作台", true, None::<&str>)?;
      let restart = MenuItem::with_id(app, "restart", "重启服务", true, None::<&str>)?;
      let quit = MenuItem::with_id(app, "quit", "退出（停止服务）", true, None::<&str>)?;
      let menu = Menu::with_items(app, &[&open, &open_ui, &restart, &quit])?;
      // 托盘图标必须显式设置，否则显示为空白（之前缺的就是这行）。
      // default_window_icon 取自 tauri.conf bundle.icon 打包进去的图标。
      let tray_icon: Option<tauri::image::Image> = app.default_window_icon().cloned();
      // 写进文件日志，方便确认图标到底有没有取到（排障用）。
      if tray_icon.is_some() {
        try_append_log("托盘图标已设置。");
      } else {
        try_append_log("警告：无默认窗口图标，托盘可能显示空白。");
      }
      let mut tray_builder = TrayIconBuilder::new().menu(&menu).tooltip("cxu 启动器");
      if let Some(icon) = tray_icon {
        tray_builder = tray_builder.icon(icon);
      }
      let _tray = tray_builder
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
          "open" => show_main(app),
          "open_ui" => {
            let svc: State<'_, ServiceCache> = app.state();
            let (running, _, port) = svc.inner.lock().unwrap().clone();
            if !running {
              try_append_log("打开工作台被忽略：服务未运行。");
              return;
            }
            let url = format!("http://{HOST}:{port}");
            try_append_log(&format!("正在系统浏览器打开工作台 {url}"));
            std::thread::spawn(move || {
              if let Err(e) = tauri_plugin_opener::open_url(url, None::<&str>) {
                try_append_log(&format!("打开浏览器失败: {e}"));
              }
            });
          }
          "restart" => {
            let h = app.clone();
            tauri::async_runtime::spawn(async move {
              let st: State<'_, Mutex<LauncherState>> = h.state();
              let cxu: State<'_, CxuCache> = h.state();
              let svc: State<'_, ServiceCache> = h.state();
              // stop then start
              {
                let mut guard = st.lock().unwrap();
                if guard.starting {
                  try_append_log("重启被忽略：正在启动中。");
                  return;
                }
                if let Some(mut child) = guard.child.take() {
                  let _ = child.kill();
                  let _ = child.wait();
                }
              }
              let _ = launcher_start(h.clone(), st, cxu, svc).await;
            });
          }
          "quit" => {
            let st: State<'_, Mutex<LauncherState>> = app.state();
            {
              let mut guard = st.lock().unwrap();
              if let Some(mut child) = guard.child.take() {
                let _ = child.kill();
                let _ = child.wait();
              }
            }
            app.exit(0);
          }
          _ => {}
        })
        .on_tray_icon_event(|tray, event| {
          if let TrayIconEvent::Click { button: MouseButton::Left, .. } = event {
            show_main(tray.app_handle());
          }
        })
        .build(app)?;
      // 后台线程（独立 OS 线程，不能用 runtime 内 block_on，会 panic）：
      // - 服务快照：每 ~2s 刷一次（快路径 1 次 TCP；只有上次端口挂了才全量扫）。
      //   所有 IPC 热路径只读快照，永不阻塞 —— 无服务时也不会卡死。
      // - cxu 定位/版本：启动立即刷一次，之后每 60s。
      // - 远端版本：首次延迟 2s 让窗口先出来，之后每 600s；失败不清零（保留旧值）。
      {
        let h2 = handle.clone();
        std::thread::spawn(move || {
          refresh_cxu_cache(&h2);
          // 启动即刷一次服务快照，窗口第一次 status 就有数，不用等。
          {
            let st: State<'_, Mutex<LauncherState>> = h2.state();
            let svc: State<'_, ServiceCache> = h2.state();
            let snap = current_service(&st);
            *svc.inner.lock().unwrap() = snap;
          }
          std::thread::sleep(Duration::from_millis(2000));
          let v = fetch_latest_blocking();
          if v.is_some() {
            let cache: State<'_, LatestCache> = h2.state();
            store_latest(&cache, v);
          }
          let mut tick: u64 = 0;
          loop {
            std::thread::sleep(Duration::from_secs(2));
            {
              let st: State<'_, Mutex<LauncherState>> = h2.state();
              let svc: State<'_, ServiceCache> = h2.state();
              let snap = current_service(&st);
              *svc.inner.lock().unwrap() = snap;
            }
            tick += 1;
            if tick % 30 == 0 {
              refresh_cxu_cache(&h2);
            }
            if tick % 300 == 0 {
              let v = fetch_latest_blocking();
              if v.is_some() {
                let cache: State<'_, LatestCache> = h2.state();
                store_latest(&cache, v);
              }
            }
          }
        });
      }
      // auto-start server if cxu exists and nothing running (best-effort, never blocks window).
      // 只在“无任何服务”时起一次；有外部服务就复用（快照里已有），绝不重复拉起。
      let h = handle.clone();
      tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let cxu: State<'_, CxuCache> = h.state();
        let svc: State<'_, ServiceCache> = h.state();
        let st: State<'_, Mutex<LauncherState>> = h.state();
        let has_cxu = cxu.inner.lock().unwrap().0.is_some() || find_cxu().is_some();
        if !has_cxu {
          return;
        }
        let (running, _, _) = svc.inner.lock().unwrap().clone();
        if running {
          return;
        }
        // 快照可能还没刷好（启动 1.5s 内），快速确认一次，避免无服务时误判。
        if probe_healthy_port().is_some() {
          return;
        }
        let _ = launcher_start(h.clone(), st, cxu, svc).await;
      });
      Ok(())
    })
    .on_window_event(|window, event| {
      if let tauri::WindowEvent::CloseRequested { api, .. } = event {
        let _ = window.hide();
        api.prevent_close();
      }
    })
    .run(tauri::generate_context!())
    .expect("cxu-launch 启动失败");
}
