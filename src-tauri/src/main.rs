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
}

impl Default for LauncherState {
  fn default() -> Self {
    Self { child: None, port: PREFERRED_PORT }
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
  let mut f = match std::fs::OpenOptions::new().create(true).append(true).open(dir.join("launcher.log")) {
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
    Some(text.replace("cxu v", "").replace('v', "").trim().to_string())
  }
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
  let Ok(mut s) = TcpStream::connect_timeout(&parsed, Duration::from_millis(800)) else {
    return false;
  };
  let _ = s.set_read_timeout(Some(Duration::from_millis(1500)));
  let req = "GET /api/runtime/health HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
  if s.write_all(req.as_bytes()).is_err() {
    return false;
  }
  let mut buf = [0u8; 512];
  match s.read(&mut buf) {
    Ok(n) => {
      let head = String::from_utf8_lossy(&buf[..n]);
      head.contains("200") || head.contains("404") || head.contains("401")
    }
    Err(_) => false,
  }
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
fn probe_healthy_port() -> Option<u16> {
  for p in PREFERRED_PORT..=(PREFERRED_PORT + 20) {
    if health_ok(p) {
      return Some(p);
    }
  }
  None
}

fn any_port_open() -> bool {
  for p in PREFERRED_PORT..=(PREFERRED_PORT + 20) {
    let addr: std::net::SocketAddr = match format!("{HOST}:{p}").parse() {
      Ok(a) => a,
      Err(_) => continue,
    };
    if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
      return true;
    }
  }
  false
}

// 统一服务状态：优先认自己的子进程；否则探测外部健康端口并接管（pid=None 表示外部进程）。
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

#[tauri::command]
fn launcher_status(
  state: State<'_, Mutex<LauncherState>>,
  cache: State<'_, LatestCache>,
  cxu: State<'_, CxuCache>,
) -> LauncherStatus {
  // 热路径：只读缓存 + 纯 socket 探测，不拉任何子进程（无黑框、无卡顿）。
  // cxu 定位/版本由后台线程慢刷；安装/更新成功后会立即刷新缓存。
  let (path, ver) = cxu.inner.lock().unwrap().clone();
  let (running, pid, port) = current_service(&state);
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
async fn launcher_start(app: AppHandle, state: State<'_, Mutex<LauncherState>>) -> Result<String, String> {
  let path = find_cxu().ok_or("未找到 cxu，请先安装。")?;
  {
    let (running, _) = child_alive(&state);
    if running {
      return Err("服务已在运行。".into());
    }
  }
  // 情况1：已有健康服务（外部手动启动 / 上次残留 / 另一实例启动），直接复用，不再起第二个。
  if let Some(p) = probe_healthy_port() {
    state.lock().unwrap().port = p;
    let url = format!("http://{HOST}:{p}");
    emit_log(&app, &format!("检测到已在运行的服务，直接复用 {url}"));
    return Ok(url);
  }
  // 情况2：端口被占用但健康检查还没通过（外部正在启动中），等 2s 再探一次，避免双实例抢跑。
  if any_port_open() {
    tokio::time::sleep(Duration::from_millis(2000)).await;
    if let Some(p) = probe_healthy_port() {
      state.lock().unwrap().port = p;
      let url = format!("http://{HOST}:{p}");
      emit_log(&app, &format!("检测到已在运行的服务，直接复用 {url}"));
      return Ok(url);
    }
  }
  let port = pick_port();
  state.lock().unwrap().port = port;
  emit_log(&app, &format!("启动 cxu: {path} serve --port {port}"));
  let _ = std::fs::create_dir_all(log_dir());
  let log_path = log_dir().join("cxu-serve.log");
  let log_file = std::fs::File::create(&log_path).map_err(|e| e.to_string())?;
  let err_file = log_file.try_clone().map_err(|e| e.to_string())?;
  let mut cmd = silent_cmd(&path);
  cmd.args(["serve", "--port", &port.to_string()]);
  cmd.stdout(Stdio::from(log_file)).stderr(Stdio::from(err_file)).stdin(Stdio::null());
  let child = cmd.spawn().map_err(|e| format!("启动失败: {e}"))?;
  let _pid = child.id();
  state.lock().unwrap().child = Some(child);
  for _ in 0..80 {
    tokio::time::sleep(Duration::from_millis(250)).await;
    let (alive, _) = child_alive(&state);
    if !alive {
      return Err("cxu 进程已退出，请看 cxu-serve.log。".into());
    }
    if health_ok(port) {
      let url = format!("http://{HOST}:{port}");
      emit_log(&app, &format!("服务就绪 {url}"));
      return Ok(url);
    }
  }
  Err("服务启动超时（20s），请看 cxu-serve.log。".into())
}

#[tauri::command]
fn launcher_stop(state: State<'_, Mutex<LauncherState>>) -> Result<(), String> {
  let mut guard = state.lock().unwrap();
  if let Some(mut child) = guard.child.take() {
    let _ = child.kill();
    let _ = child.wait();
  }
  Ok(())
}

#[tauri::command]
async fn launcher_restart(app: AppHandle, state: State<'_, Mutex<LauncherState>>) -> Result<String, String> {
  {
    let mut guard = state.lock().unwrap();
    if let Some(mut child) = guard.child.take() {
      let _ = child.kill();
      let _ = child.wait();
    }
  }
  tokio::time::sleep(Duration::from_millis(500)).await;
  launcher_start(app, state).await
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
fn launcher_open_ui(app: AppHandle, state: State<'_, Mutex<LauncherState>>) -> Result<(), String> {
  // 点击即返回：只读上次已知端口（由轮询 + 启动时维护），绝不在点击路径上做全端口扫描；
  // 浏览器拉起丢独立线程 fire-and-forget，任何情况下都不卡 IPC、不卡窗口。
  let port = state.lock().unwrap().port;
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
  tauri::Builder::default()
    .manage(state)
    .manage(latest)
    .manage(cxu)
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
            let st: State<'_, Mutex<LauncherState>> = app.state();
            let port = st.lock().unwrap().port;
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
              // stop then start
              {
                let mut guard = st.lock().unwrap();
                if let Some(mut child) = guard.child.take() {
                  let _ = child.kill();
                  let _ = child.wait();
                }
              }
              let _ = launcher_start(h.clone(), st).await;
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
      // 后台慢刷（独立 OS 线程，不能用 runtime 内 block_on，会 panic）：
      // - cxu 定位/版本：启动立即刷一次，之后每 60s（装/卸 cxu 才变，不值得进 3s 热路径）。
      // - 远端版本：首次延迟 2s 让窗口先出来，之后每 600s；失败不清零（保留旧值）。
      {
        let h2 = handle.clone();
        std::thread::spawn(move || {
          refresh_cxu_cache(&h2);
          std::thread::sleep(Duration::from_millis(2000));
          let v = fetch_latest_blocking();
          if v.is_some() {
            let cache: State<'_, LatestCache> = h2.state();
            store_latest(&cache, v);
          }
          let mut tick: u64 = 0;
          loop {
            std::thread::sleep(Duration::from_secs(30));
            tick += 1;
            if tick % 2 == 0 {
              refresh_cxu_cache(&h2);
            }
            if tick % 20 == 0 {
              let v = fetch_latest_blocking();
              if v.is_some() {
                let cache: State<'_, LatestCache> = h2.state();
                store_latest(&cache, v);
              }
            }
          }
        });
      }
      // auto-start server if cxu exists (best-effort, never blocks window)
      let h = handle.clone();
      tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(600)).await;
        if find_cxu().is_none() {
          return;
        }
        let st: State<'_, Mutex<LauncherState>> = h.state();
        let _ = launcher_start(h.clone(), st).await;
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
