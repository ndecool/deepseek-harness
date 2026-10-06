//! Native Linux shell for DeepSeek Harness: supervises `dsh web` and shows its Web UI in WebKitGTK.

mod runtime;
mod shell_env;

use runtime::{Phase, StatusSnapshot, Supervisor};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::webview::NewWindowResponse;
use tauri::{AppHandle, Manager, RunEvent, State, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_opener::OpenerExt;

const MAIN: &str = "main";
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(10);

/// Origin of the local status page, captured before the first navigation.
struct StatusPage(Url);

#[tauri::command]
fn runtime_status(supervisor: State<'_, Arc<Supervisor>>) -> StatusSnapshot {
    supervisor.snapshot()
}

#[tauri::command]
fn restart_runtime(supervisor: State<'_, Arc<Supervisor>>) {
    let supervisor = Arc::clone(&supervisor);
    std::thread::spawn(move || supervisor.restart());
}

#[tauri::command]
fn open_log(app: AppHandle, supervisor: State<'_, Arc<Supervisor>>) -> Result<(), String> {
    app.opener().open_path(supervisor.snapshot().log_path, None::<&str>).map_err(|e| e.to_string())
}

fn main() {
    prefer_working_webkit_renderer();
    tauri::Builder::default()
        // Must be first: a second launch (or a `dsh://` URL) focuses the running instance instead.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| focus_main(app)))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .invoke_handler(tauri::generate_handler![runtime_status, restart_runtime, open_log])
        .setup(|app| {
            let handle = app.handle().clone();
            if std::env::var_os("APPIMAGE").is_some() {
                // AppImages carry no installed .desktop entry; register the dsh:// handler at runtime.
                let _ = app.deep_link().register_all();
            }
            let deep_link_handle = handle.clone();
            app.deep_link().on_open_url(move |_| focus_main(&deep_link_handle));

            let window = build_main_window(&handle)?;
            app.manage(StatusPage(window.url()?));

            let (env, failures) = shell_env::login_environment(LOGIN_SHELL_TIMEOUT);
            let log_path = state_dir().join("dsh.log");
            let resource_dir = handle.path().resource_dir().ok();
            let change_handle = handle.clone();
            let supervisor = Supervisor::new(
                resource_dir,
                env,
                log_path,
                Box::new(move |phase| on_phase(&change_handle, phase)),
            );
            for failure in failures {
                eprintln!("deepseek-harness: login shell environment unavailable from {failure}");
            }
            app.manage(Arc::clone(&supervisor));
            if tray_supported() {
                build_tray(&handle)?;
            }
            std::thread::spawn(move || supervisor.restart());
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build DeepSeek Harness")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                app.state::<Arc<Supervisor>>().shutdown();
            }
        });
}

fn build_main_window(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let navigation_app = app.clone();
    let popup_app = app.clone();
    WebviewWindowBuilder::new(app, MAIN, WebviewUrl::App("index.html".into()))
        .title("DeepSeek Harness")
        .inner_size(1280.0, 820.0)
        .min_inner_size(720.0, 480.0)
        .center()
        // Only the status page and the running dsh origin load in-app; everything else opens in the browser.
        .on_navigation(move |url| {
            if is_in_app(&navigation_app, url) {
                return true;
            }
            open_external(&navigation_app, url);
            false
        })
        .on_new_window(move |url, _features| {
            open_external(&popup_app, &url);
            NewWindowResponse::Deny
        })
        .build()
}

fn is_in_app(app: &AppHandle, url: &Url) -> bool {
    if url.scheme() == "about" {
        return true;
    }
    if let Some(page) = app.try_state::<StatusPage>() {
        if url.origin() == page.0.origin() {
            return true;
        }
    } else {
        // The status page itself loads before its origin is recorded.
        return url.scheme() == "tauri";
    }
    match app.try_state::<Arc<Supervisor>>().map(|s| s.phase()) {
        Some(Phase::Ready { url: ready }) => Url::parse(&ready).is_ok_and(|ready| ready.origin() == url.origin()),
        _ => false,
    }
}

fn open_external(app: &AppHandle, url: &Url) {
    if matches!(url.scheme(), "http" | "https" | "mailto") {
        let _ = app.opener().open_url(url.as_str(), None::<&str>);
    }
}

/// Ready: show the dsh Web UI. Starting or failed: show the local status page, which polls status.
fn on_phase(app: &AppHandle, phase: &Phase) {
    let Some(window) = app.get_webview_window(MAIN) else { return };
    let target = match phase {
        Phase::Ready { url } => Url::parse(url).ok(),
        Phase::Starting | Phase::Failed { .. } => app.try_state::<StatusPage>().map(|page| page.0.clone()),
    };
    let Some(target) = target else { return };
    let on_status_page = app
        .try_state::<StatusPage>()
        .zip(window.url().ok())
        .is_some_and(|(page, current)| current.origin() == page.0.origin());
    // The status page re-renders itself from polling; reloading it would only flash.
    if matches!(phase, Phase::Ready { .. }) || !on_status_page {
        let _ = window.navigate(target);
    }
}

fn focus_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show DeepSeek Harness", true, None::<&str>)?;
    let restart = MenuItem::with_id(app, "restart", "Restart runtime", true, None::<&str>)?;
    let log = MenuItem::with_id(app, "log", "Open runtime log", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&show, &restart, &log, &separator, &quit])?;
    TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().cloned().expect("bundled window icon"))
        .tooltip("DeepSeek Harness")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => focus_main(app),
            "restart" => {
                let supervisor = Arc::clone(&app.state::<Arc<Supervisor>>());
                std::thread::spawn(move || supervisor.restart());
            }
            "log" => {
                let path = app.state::<Arc<Supervisor>>().snapshot().log_path;
                let _ = app.opener().open_path(path, None::<&str>);
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

/// libappindicator is loaded at runtime and its absence aborts the process, so probe it first.
fn tray_supported() -> bool {
    ["libayatana-appindicator3.so.1", "libappindicator3.so.1"].iter().any(|name| {
        let name = std::ffi::CString::new(*name).expect("static name");
        // SAFETY: dlopen/dlclose on a NUL-terminated name; the handle is closed immediately.
        unsafe {
            let handle = libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
            if handle.is_null() {
                return false;
            }
            libc::dlclose(handle);
            true
        }
    })
}

/// `$XDG_STATE_HOME/deepseek-harness`, defaulting to `~/.local/state/deepseek-harness`.
fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state")))
        .unwrap_or_else(std::env::temp_dir)
        .join("deepseek-harness")
}

/// WebKitGTK's DMA-BUF renderer shows a blank window on the NVIDIA proprietary driver.
fn prefer_working_webkit_renderer() {
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() && std::path::Path::new("/proc/driver/nvidia").exists() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
}
