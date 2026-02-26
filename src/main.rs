#![allow(unexpected_cfgs)]

use block::ConcreteBlock;
use chrono;
use cocoa::base::{id, nil};
use cocoa::foundation::NSString;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{Boolean, CFTypeRef, TCFType, TCFTypeRef};
use core_foundation::runloop::CFRunLoopRunInMode;
use core_foundation::string::{CFString, CFStringRef};
use fern;
use log::{error, info, warn};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use objc::{class, msg_send, sel, sel_impl};
use parking_lot::Mutex;
use serde::Deserialize;
use std::collections::HashMap;
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::sync::mpsc::{channel, Sender};

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn TISCopyCurrentKeyboardInputSource() -> CFTypeRef;
    fn TISCreateInputSourceList(
        properties: *const c_void,
        include_all_installed: Boolean,
    ) -> CFArrayRef;
    fn TISGetInputSourceProperty(input_source: CFTypeRef, property_key: CFStringRef) -> CFTypeRef;
    fn TISSelectInputSource(input_source: CFTypeRef);
}

const K_TIS_PROPERTY_INPUT_SOURCE_ID: &str = "TISPropertyInputSourceID";
const K_CF_RUN_LOOP_DEFAULT_MODE: &str = "kCFRunLoopDefaultMode";

#[derive(Debug, Deserialize, PartialEq, Clone, Default)]
struct AppConfig {
    #[serde(default)]
    app: HashMap<String, String>,
    #[serde(default)]
    debug: DebugConfig,
}

#[derive(Debug, Deserialize, PartialEq, Clone)]
struct DebugConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "DebugConfig::default_level")]
    level: String,
    #[serde(default = "DebugConfig::default_file")]
    file: String,
}

impl DebugConfig {
    fn default_level() -> String {
        "info".to_string()
    }

    fn default_file() -> String {
        let base = std::env::var("XDG_CONFIG_HOME")
            .or_else(|_| std::env::var("HOME").map(|h| format!("{}/.config", h)))
            .unwrap_or_else(|_| ".".to_string());
        format!("{}/iswitch/iswitch.log", base)
    }

    fn as_level_filter(&self) -> log::LevelFilter {
        match self.level.to_lowercase().as_str() {
            "trace" => log::LevelFilter::Trace,
            "debug" => log::LevelFilter::Debug,
            "warn" => log::LevelFilter::Warn,
            "error" => log::LevelFilter::Error,
            _ => log::LevelFilter::Info,
        }
    }
}

impl Default for DebugConfig {
    fn default() -> Self {
        DebugConfig {
            enabled: false,
            level: Self::default_level(),
            file: Self::default_file(),
        }
    }
}

/// Build a base `fern::Dispatch` with timestamp + level formatting.
/// `chain_targets` lets callers add stdout / file sinks declaratively.
fn base_dispatch(level: log::LevelFilter) -> fern::Dispatch {
    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} [{}] [{}] {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                record.level(),
                record.target(),
                message
            ))
        })
        .level(level)
}

fn setup_logging(config: &AppConfig) -> Result<(), fern::InitError> {
    if !config.debug.enabled {
        return base_dispatch(log::LevelFilter::Info)
            .chain(std::io::stdout())
            .apply()
            .map_err(fern::InitError::SetLoggerError);
    }

    let level = config.debug.as_level_filter();
    let log_file = config
        .debug
        .file
        .replace('~', &std::env::var("HOME").unwrap_or_default());

    // Ensure directory exists; fall back to stdout-only on failure.
    if let Some(parent) = Path::new(&log_file).parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).unwrap_or_else(|e| {
                eprintln!("Cannot create log dir: {}", e);
            });
        }
    }

    let dispatch = base_dispatch(level).chain(std::io::stdout());

    // Attempt to add a file sink; silently degrade to stdout only.
    let dispatch = match fern::log_file(&log_file) {
        Ok(file) => dispatch.chain(file),
        Err(e) => {
            eprintln!("Cannot open log file {}: {} — stdout only", log_file, e);
            dispatch
        }
    };

    dispatch.apply().map_err(fern::InitError::SetLoggerError)
}

unsafe fn tis_string_property_fixed(source: CFTypeRef, key: &str) -> Option<String> {
    let ptr = TISGetInputSourceProperty(source, CFString::new(key).as_concrete_TypeRef());
    if ptr.is_null() {
        return None;
    }
    let cf: CFString = TCFType::wrap_under_get_rule(ptr as CFStringRef);
    Some(cf.to_string())
}

/// Iterate over all installed input sources as an owned `Vec<CFTypeRef>`.
/// Callers are responsible for keeping the original `CFArray` alive.
unsafe fn all_input_sources() -> Option<(CFArray, Vec<CFTypeRef>)> {
    let ref_ = TISCreateInputSourceList(ptr::null(), 0);
    if ref_.is_null() {
        return None;
    }
    let arr: CFArray = TCFType::wrap_under_get_rule(ref_);
    let sources = (0..arr.len())
        .filter_map(|i| {
            arr.get(i as isize)
                .map(|item| item.as_void_ptr() as CFTypeRef)
        })
        .filter(|p| !p.is_null())
        .collect::<Vec<_>>();
    Some((arr, sources))
}

fn get_current_input_source() -> String {
    unsafe {
        let src = TISCopyCurrentKeyboardInputSource();
        if src.is_null() {
            warn!("Current input source is null");
            return "Unknown".to_string();
        }
        tis_string_property_fixed(src, K_TIS_PROPERTY_INPUT_SOURCE_ID)
            .unwrap_or_else(|| "Unknown".to_string())
    }
}

unsafe fn find_source_by_id<'a>(sources: &'a [CFTypeRef], target: &str) -> Option<CFTypeRef> {
    let target_cf = CFString::new(target);
    sources.iter().copied().find(|&src| {
        tis_string_property_fixed(src, K_TIS_PROPERTY_INPUT_SOURCE_ID)
            .map(|id| CFString::new(&id) == target_cf)
            .unwrap_or(false)
    })
}

fn switch_input_source(target_id: &str) {
    unsafe {
        let Some((_arr, sources)) = all_input_sources() else {
            error!("Failed to get input source list");
            return;
        };

        match find_source_by_id(&sources, target_id) {
            Some(src) => {
                info!("Switching to input source: {}", target_id);
                TISSelectInputSource(src);
            }
            None => warn!("Input source '{}' not found", target_id),
        }
    }
}

unsafe fn source_display_name(source: CFTypeRef) -> String {
    // Chain of fallback properties, evaluated lazily.
    tis_string_property_fixed(source, "TISPropertyLocalizedName")
        .or_else(|| tis_string_property_fixed(source, "TISPropertyInputModeID"))
        .or_else(|| {
            // Try first language in the languages array
            let ptr = TISGetInputSourceProperty(
                source,
                CFString::new("TISPropertyInputSourceLanguages").as_concrete_TypeRef(),
            );
            if ptr.is_null() {
                return None;
            }
            let langs: CFArray = TCFType::wrap_under_get_rule(ptr as CFArrayRef);
            langs.get(0).and_then(|item| {
                let p = item.as_void_ptr() as CFStringRef;
                if p.is_null() {
                    return None;
                }
                let cf: CFString = TCFType::wrap_under_get_rule(p);
                Some(cf.to_string())
            })
        })
        .or_else(|| {
            tis_string_property_fixed(source, "TISPropertyInputSourceType")
                .map(|t| format!("Unknown ({})", t))
        })
        .unwrap_or_else(|| "Unknown".to_string())
}

fn print_available_input_sources() {
    unsafe {
        let Some((_arr, sources)) = all_input_sources() else {
            println!("Failed to get input source list");
            return;
        };

        println!("Available input sources:");
        for src in sources {
            let id = tis_string_property_fixed(src, K_TIS_PROPERTY_INPUT_SOURCE_ID)
                .unwrap_or_else(|| "(no id)".to_string());
            let name = source_display_name(src);
            println!("{} - {}", id, name);
        }
    }
}

fn config_path() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("iswitch/config.toml")
}

const DEFAULT_CONFIG_TOML: &str = r#"[app]
# Terminal = "com.apple.keylayout.ABC"

[debug]
enabled = false
level = "info"
file = "~/.config/iswitch/iswitch.log"
"#;

async fn load_or_create_config(path: &Path) -> AppConfig {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent).await;
        }
        let _ = fs::write(path, DEFAULT_CONFIG_TOML).await;
        info!("Created default config at: {}", path.display());
    }

    fs::read_to_string(path)
        .await
        .map_err(|e| {
            error!("Cannot read config: {}", e);
            e
        })
        .ok()
        .and_then(|s| {
            toml::from_str::<AppConfig>(&s)
                .map_err(|e| error!("Cannot parse config: {}", e))
                .ok()
        })
        .unwrap_or_default()
}

fn is_dev_mode() -> bool {
    std::env::var("CARGO").is_ok()
        || std::env::var("ISWITCH_DEV").is_ok()
        || std::env::current_exe()
            .map(|p| p.to_string_lossy().contains("target/debug"))
            .unwrap_or(false)
}

fn is_already_running() -> bool {
    let my_pid = std::process::id();
    Command::new("pgrep")
        .args(["-f", "iswitch"])
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                .any(|pid| pid != my_pid)
        })
        .unwrap_or(false)
}

unsafe fn safe_nsstring(obj: id) -> Option<String> {
    if obj.is_null() {
        return None;
    }
    let ptr: *const std::os::raw::c_char = msg_send![obj, UTF8String];
    if ptr.is_null() {
        return None;
    }
    Some(std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned())
}

fn desired_input_source<'a>(
    app_name: &str,
    config: &'a AppConfig,
    current: &str,
) -> Option<&'a str> {
    config
        .app
        .get(app_name)
        .map(|s| s.as_str())
        .filter(|&target| !current.contains(target))
}

async fn watch_config(config_path: PathBuf, tx: Sender<()>, last: Arc<Mutex<Instant>>) {
    let debounce = Duration::from_millis(500);
    let (inner_tx, mut inner_rx) = channel::<()>(1);

    let mut watcher = match RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            if matches!(
                res,
                Ok(Event {
                    kind: EventKind::Modify(_),
                    ..
                })
            ) {
                let now = Instant::now();
                let mut guard = last.lock();
                if now.duration_since(*guard) > debounce {
                    *guard = now;
                    let _ = inner_tx.try_send(());
                }
            }
        },
        Config::default(),
    ) {
        Ok(w) => w,
        Err(e) => {
            error!("Watcher init failed: {}", e);
            return;
        }
    };

    if let Err(e) = watcher.watch(&config_path, RecursiveMode::NonRecursive) {
        error!("Failed to watch {:?}: {}", config_path, e);
        return;
    }

    while inner_rx.recv().await.is_some() {
        if tx.send(()).await.is_err() {
            break;
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.get(1).map(|s| s.as_str()) == Some("-p") {
        print_available_input_sources();
        return;
    }

    let path = config_path();
    let mut config = load_or_create_config(&path).await;

    if is_dev_mode() {
        config.debug.enabled = true;
        config.debug.level = "trace".to_string();
    }

    if let Err(e) = setup_logging(&config) {
        eprintln!("Logging setup failed: {}", e);
    }

    info!("iSwitch starting");

    match args.get(1).map(|s| s.as_str()) {
        Some("-s") => {
            let target = match args.get(2) {
                Some(t) => t,
                None => {
                    error!("Usage: -s <input_source_id>");
                    return;
                }
            };
            let current = get_current_input_source();
            if current != *target {
                switch_input_source(target);
            }
            return; // always exit after -s
        }
        Some("-v") | Some("--version") => {
            println!("iswitch {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("-h") | Some("--help") => {
            println!("iSwitch — auto-switch input sources based on active app\n");
            println!("USAGE:");
            println!("  iswitch              Daemon mode");
            println!("  iswitch -s <id>      Switch to input source");
            println!("  iswitch -p           List input sources");
            println!("  iswitch -v           Show version");
            println!("  iswitch -h           Help");
            return;
        }
        Some(flag) => {
            error!("Unknown flag: {}. Use -h.", flag);
            return;
        }
        None => {} // daemon mode
    }

    if is_already_running() {
        error!("Another iswitch daemon is running. Exiting.");
        return;
    }

    info!("Starting daemon mode");

    let config = Arc::new(Mutex::new(config));
    let last_update = Arc::new(Mutex::new(Instant::now()));
    let (tx, mut rx) = channel::<()>(10);

    tokio::spawn(watch_config(path.clone(), tx, last_update.clone()));

    unsafe {
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let nc: id = msg_send![workspace, notificationCenter];

        if workspace.is_null() || nc.is_null() {
            error!("Failed to get NSWorkspace or notification center");
            return;
        }

        let block = ConcreteBlock::new({
            let config = Arc::clone(&config);
            move |notification: id| {
                // Use Option chaining to keep the happy-path linear
                let app_name = (|| -> Option<String> {
                    let info: id = msg_send![notification, userInfo];
                    let key = NSString::alloc(nil).init_str("NSWorkspaceApplicationKey");
                    let running_app: id = msg_send![info, objectForKey: key];
                    safe_nsstring(msg_send![running_app, localizedName])
                })();

                let Some(app_name) = app_name else {
                    eprintln!("Could not read application name from notification");
                    return;
                };

                info!("Active app: {}", app_name);

                let current = get_current_input_source();
                let target = {
                    let cfg = config.lock();
                    desired_input_source(&app_name, &cfg, &current).map(|s| s.to_owned())
                };

                if let Some(t) = target {
                    info!("Switching to {} for {}", t, app_name);
                    switch_input_source(&t);
                }
            }
        });
        let block = &*block.copy();

        let name = NSString::alloc(nil).init_str("NSWorkspaceDidActivateApplicationNotification");
        let _: id = msg_send![nc,
            addObserverForName: name
            object: nil
            queue: nil
            usingBlock: block
        ];

        info!("Listening for app-switch events");

        loop {
            if rx.try_recv().is_ok() {
                info!("Reloading config");
                let new_cfg = load_or_create_config(&path).await;
                let mut cur = config.lock();
                if *cur != new_cfg {
                    if cur.debug != new_cfg.debug {
                        let _ = setup_logging(&new_cfg);
                    }
                    *cur = new_cfg;
                    info!("Config reloaded");
                }
            }

            CFRunLoopRunInMode(
                CFString::new(K_CF_RUN_LOOP_DEFAULT_MODE).as_concrete_TypeRef(),
                0.5,
                false as u8,
            );
        }
    }
}
