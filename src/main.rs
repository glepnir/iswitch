#![allow(unexpected_cfgs)]

use block::ConcreteBlock;
use cocoa::base::{id, nil};
use cocoa::foundation::NSString;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{Boolean, CFTypeRef, TCFType, TCFTypeRef};
use core_foundation::runloop::CFRunLoopRunInMode;
use core_foundation::string::{CFString, CFStringRef};
use lazy_static::lazy_static;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use objc::{class, msg_send, sel, sel_impl};
use parking_lot::Mutex; // Using parking_lot::Mutex instead of std::sync::Mutex
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
// Logging related imports
use chrono;
use fern;
use log::{debug, error, info, trace, warn};

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

// Thread-safe app context using lazy_static and Arc<Mutex<>>
lazy_static! {
    static ref APP_CONTEXT: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
}

// Enhanced app context handling with parking_lot::Mutex
fn set_app_context(app_name: Option<String>) {
    // parking_lot::Mutex doesn't poison on panic
    *APP_CONTEXT.lock() = app_name;
}

fn get_app_context() -> Option<String> {
    // parking_lot::Mutex doesn't poison on panic
    APP_CONTEXT.lock().clone()
}

#[derive(Debug, Deserialize, PartialEq, Clone)]
struct AppConfig {
    app: HashMap<String, String>,
    #[serde(default)]
    debug: DebugConfig,
}

#[derive(Debug, Deserialize, PartialEq, Clone)]
struct DebugConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_log_level")]
    level: String,
    #[serde(default = "default_log_file")]
    file: String,
}

impl Default for DebugConfig {
    fn default() -> Self {
        DebugConfig {
            enabled: false,
            level: default_log_level(),
            file: default_log_file(),
        }
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_log_file() -> String {
    // Use XDG_CONFIG_HOME for log file path if available
    if let Ok(config_dir) = std::env::var("XDG_CONFIG_HOME") {
        format!("{}/iswitch/iswitch.log", config_dir)
    } else {
        // Fallback to HOME directory if XDG_CONFIG_HOME is not set
        let home_dir = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{}/.config/iswitch/iswitch.log", home_dir)
    }
}

fn log_format(out: fern::FormatCallback, message: &std::fmt::Arguments, record: &log::Record) {
    // Safely get app context with a default fallback, without any unwrap
    let app_context = match get_app_context() {
        Some(ctx) => ctx,
        None => "iswitch".to_string(),
    };

    // Create timestamp string with safe formatting
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    // Use format_args! directly instead of format! to avoid allocation failures
    let _ = out.finish(format_args!(
        "{} [{}] [{}] [{}] {}",
        timestamp,
        record.level(),
        record.target(),
        app_context,
        message
    ));
}

// Improved console-only logging setup function
fn setup_console_only_logging() -> Result<(), fern::InitError> {
    // Use a simpler format function for fallback logging
    let fallback_format =
        |out: fern::FormatCallback, message: &std::fmt::Arguments, record: &log::Record| {
            let _ = out.finish(format_args!(
                "[{}] [{}] {}",
                record.level(),
                record.target(),
                message
            ));
        };

    match fern::Dispatch::new()
        .format(fallback_format)
        .level(log::LevelFilter::Info)
        .chain(std::io::stdout())
        .apply()
    {
        Ok(_) => {
            eprintln!("Initialized console-only logging");
            Ok(())
        }
        Err(e) => {
            eprintln!("Failed to initialize console logging: {}", e);
            // Convert SetLoggerError to InitError
            Err(fern::InitError::SetLoggerError(e))
        }
    }
}

// Fixed setup_logging function with proper error handling
fn setup_logging(config: &AppConfig) -> Result<(), fern::InitError> {
    if !config.debug.enabled {
        // If debugging is not enabled, only configure basic console logging
        return setup_console_only_logging();
    }

    // Parse log level with safe defaults
    let log_level = match config.debug.level.to_lowercase().as_str() {
        "trace" => log::LevelFilter::Trace,
        "debug" => log::LevelFilter::Debug,
        "info" => log::LevelFilter::Info,
        "warn" => log::LevelFilter::Warn,
        "error" => log::LevelFilter::Error,
        _ => log::LevelFilter::Info, // Safe default
    };

    // Ensure log file directory exists with proper error handling
    let log_file = match std::env::var("HOME") {
        Ok(home) => config.debug.file.replace("~", &home),
        Err(_) => config.debug.file.clone(), // Keep as is if HOME not available
    };

    if let Some(parent) = Path::new(&log_file).parent() {
        if !parent.exists() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("Failed to create log directory: {}", e);
                return setup_console_only_logging();
            }
        }
    }

    // Create dispatch with safe error handling for both stdout and file
    let dispatch = fern::Dispatch::new().format(log_format).level(log_level);

    // First try to set up stdout logging, this should rarely fail
    let stdout_dispatch = dispatch.chain(std::io::stdout());

    // Then try to set up file logging, but don't fail if it doesn't work
    match fern::log_file(&log_file) {
        Ok(file) => {
            match stdout_dispatch.chain(file).apply() {
                Ok(_) => {
                    // Try to log initialization but don't rely on it
                    match std::panic::catch_unwind(|| {
                        info!(
                            "Log system initialized, level: {}, log file: {}",
                            config.debug.level, log_file
                        );
                    }) {
                        Err(_) => eprintln!("Warning: Failed to log initialization message"),
                        _ => {}
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("Failed to setup combined logging: {}", e);
                    // Convert SetLoggerError to InitError
                    Err(fern::InitError::SetLoggerError(e))
                }
            }
        }
        Err(e) => {
            eprintln!("Failed to open log file {}: {}", log_file, e);
            // Just use stdout logging as fallback
            match stdout_dispatch.apply() {
                Ok(_) => {
                    eprintln!("Falling back to console-only logging");
                    Ok(())
                }
                Err(e) => {
                    eprintln!("Failed to initialize console logging: {}", e);
                    // Convert SetLoggerError to InitError
                    Err(fern::InitError::SetLoggerError(e))
                }
            }
        }
    }
}

fn get_config_path() -> String {
    let config_path = if let Ok(config_dir) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(config_dir)
    } else if let Ok(home_dir) = std::env::var("HOME") {
        let mut path = PathBuf::from(home_dir);
        path.push(".config");
        path
    } else {
        // Instead of panicking, use a reasonable default
        eprintln!("Neither XDG_CONFIG_HOME nor HOME environment variables are set, using current directory");
        PathBuf::from(".")
    };

    let mut final_path = config_path;
    final_path.push("iswitch");
    final_path.push("config.toml");

    final_path.to_string_lossy().to_string()
}

async fn ensure_then_load(config_path: &str) -> AppConfig {
    let path = Path::new(config_path);

    // Default config as a fallback
    let default_config = AppConfig {
        app: HashMap::new(),
        debug: DebugConfig::default(),
    };

    // Only print config diagnostics in debug mode
    if log::log_enabled!(log::Level::Debug) {
        debug!("Config path: {}, exist: {}", config_path, path.exists());
    }

    if !path.exists() {
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent).await {
                error!("Failed to create config directory: {}", e);
                // Return a default config instead of panicking
                return default_config;
            }
        }

        // Richer default configuration, including debug section
        let default_config_str = r#"[app]
# Examples of application-to-input-source mappings
# Format: "Application Name" = "InputSourceID"
# 
# "Terminal" = "com.apple.keylayout.US"
# "Safari" = "com.apple.inputmethod.TCIM.Pinyin"

[debug]
# Enable debugging
enabled = false
# Log level: trace, debug, info, warn, error
level = "info"
# Log file path
file = "~/Library/Logs/iswitch.log"
"#;
        if let Err(e) = fs::write(path, default_config_str).await {
            error!("Failed to write default config file: {}", e);
            // Return a default config instead of panicking
            return default_config;
        }
        info!("Created default config file at: {}", config_path);
    }

    match fs::read_to_string(config_path).await {
        Ok(config_content) => {
            // Only print full config content in debug mode
            if log::log_enabled!(log::Level::Debug) {
                debug!("Config content read: {}", config_content);
            }

            match toml::from_str::<AppConfig>(&config_content) {
                Ok(parsed_config) => {
                    // Only log detailed app mappings in debug mode
                    if log::log_enabled!(log::Level::Debug) {
                        debug!("Loaded app mappings:");
                        for (app, input_source) in &parsed_config.app {
                            debug!("  {} -> {}", app, input_source);
                        }
                        debug!(
                            "Debug settings: enabled={}, level={}, file={}",
                            parsed_config.debug.enabled,
                            parsed_config.debug.level,
                            parsed_config.debug.file
                        );
                    }
                    parsed_config
                }
                Err(e) => {
                    error!("Failed to parse config file: {}", e);
                    error!("Config content: {}", config_content);
                    // Return a default config instead of panicking
                    default_config
                }
            }
        }
        Err(e) => {
            error!("Unable to read config file: {}", e);
            // Return a default config instead of panicking
            default_config
        }
    }
}

// Improved function with better error handling
fn get_current_input_source() -> String {
    unsafe {
        let current_source = TISCopyCurrentKeyboardInputSource();
        if current_source.is_null() {
            warn!("Current input source is null");
            return String::from("Unknown");
        }

        let source_name = TISGetInputSourceProperty(
            current_source,
            CFString::new(K_TIS_PROPERTY_INPUT_SOURCE_ID).as_concrete_TypeRef(),
        );

        if source_name.is_null() {
            warn!("Source name property is null");
            return String::from("Unknown");
        }

        let cf_str: CFString = TCFType::wrap_under_get_rule(source_name as CFStringRef);
        let source_id = cf_str.to_string();
        debug!("Current input source ID: {}", source_id);
        source_id
    }
}

fn switch_input_source(input_source_id: &str) {
    let input_source_cfstring = CFString::new(input_source_id);

    unsafe {
        let source_list_ref = TISCreateInputSourceList(ptr::null(), 0);

        if source_list_ref.is_null() {
            error!("Failed to get input source list");
            return;
        }

        let source_list: CFArray = TCFType::wrap_under_get_rule(source_list_ref);

        info!("Attempting to switch to input source: {}", input_source_id);

        let mut found = false;
        for i in 0..source_list.len() {
            let source = match source_list.get(i as isize) {
                Some(item) => item.as_void_ptr() as CFTypeRef,
                None => continue, // Skip invalid entries safely
            };

            if source.is_null() {
                continue; // Skip null sources safely
            }

            let source_id_ptr = TISGetInputSourceProperty(
                source as CFTypeRef,
                CFString::new(K_TIS_PROPERTY_INPUT_SOURCE_ID).as_concrete_TypeRef(),
            );

            if source_id_ptr.is_null() {
                continue; // Skip entries with null properties
            }

            let source_cfstring: CFString =
                TCFType::wrap_under_get_rule(source_id_ptr as CFStringRef);

            if source_cfstring == input_source_cfstring {
                info!("Found matching input source, switching...");
                TISSelectInputSource(source);
                found = true;
                break;
            }
        }

        if !found {
            warn!("Warning: Input source '{}' not found", input_source_id);
        }
    }
}

async fn watch_config_for_changes(
    config_path: String,
    tx: Sender<()>,
    last_update_time: Arc<Mutex<Instant>>,
) {
    let debounce_duration = Duration::from_millis(500);

    let (watcher_tx, mut watcher_rx) = channel(1);

    let mut watcher = match RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            if let Ok(Event {
                kind: EventKind::Modify(_),
                ..
            }) = res
            {
                // parking_lot::Mutex doesn't poison on panic
                let now = Instant::now();
                let mut last_update = last_update_time.lock();
                if now.duration_since(*last_update) > debounce_duration {
                    *last_update = now;
                    if watcher_tx.try_send(()).is_err() {
                        // This is normal when shutting down, so debug level is appropriate
                        debug!("Failed to send file event update - channel may be closed");
                    }
                }
            }
        },
        Config::default(),
    ) {
        Ok(w) => w,
        Err(e) => {
            error!("Failed to create watcher: {}", e);
            return; // Exit the function instead of panicking
        }
    };

    match watcher.watch(Path::new(&config_path), RecursiveMode::NonRecursive) {
        Ok(_) => info!("Started watching config file: {}", config_path),
        Err(e) => {
            error!("Failed to watch config file {}: {}", config_path, e);
            return; // Exit the function instead of panicking
        }
    };

    while let Some(()) = watcher_rx.recv().await {
        debug!("Config file change detected");
        if let Err(e) = tx.send(()).await {
            error!("Failed to send config reload signal: {}", e);
            break; // Break the loop if the channel is closed
        }
    }

    debug!("Config watcher task ended");
}

fn print_available_input_sources() {
    unsafe {
        let source_list_ref = TISCreateInputSourceList(ptr::null(), 0);

        if source_list_ref.is_null() {
            println!("Failed to get input source list");
            return;
        }

        let source_list: CFArray = TCFType::wrap_under_get_rule(source_list_ref);

        println!("Available input sources:");
        for i in 0..source_list.len() {
            let source = match source_list.get(i as isize) {
                Some(item) => item.as_void_ptr() as CFTypeRef,
                None => continue, // Skip invalid items safely
            };

            if source.is_null() {
                continue; // Skip null sources safely
            }

            let source_id_ptr = TISGetInputSourceProperty(
                source as CFTypeRef,
                CFString::new(K_TIS_PROPERTY_INPUT_SOURCE_ID).as_concrete_TypeRef(),
            );

            if source_id_ptr.is_null() {
                continue; // Skip entries with null id properties
            }

            let source_cfstring: CFString =
                TCFType::wrap_under_get_rule(source_id_ptr as CFStringRef);

            let localized_name_ptr = TISGetInputSourceProperty(
                source,
                CFString::new("TISPropertyLocalizedName").as_concrete_TypeRef(),
            );

            let localized_name_str = if !localized_name_ptr.is_null() {
                let localized_cfstring: CFString =
                    TCFType::wrap_under_get_rule(localized_name_ptr as CFStringRef);
                localized_cfstring.to_string()
            } else {
                let input_mode_id_ptr = TISGetInputSourceProperty(
                    source,
                    CFString::new("TISPropertyInputModeID").as_concrete_TypeRef(),
                );

                if !input_mode_id_ptr.is_null() {
                    let input_mode_cfstring: CFString =
                        TCFType::wrap_under_get_rule(input_mode_id_ptr as CFStringRef);
                    input_mode_cfstring.to_string()
                } else {
                    let input_languages_ptr = TISGetInputSourceProperty(
                        source,
                        CFString::new("TISPropertyInputSourceLanguages").as_concrete_TypeRef(),
                    );

                    if !input_languages_ptr.is_null() {
                        let languages: CFArray =
                            TCFType::wrap_under_get_rule(input_languages_ptr as CFArrayRef);

                        if languages.len() > 0 {
                            let lang = languages.get(0).map_or("Unknown".to_string(), |item| {
                                let lang_ptr: *const c_void = item.as_void_ptr();
                                if !lang_ptr.is_null() {
                                    let lang_str: CFString =
                                        TCFType::wrap_under_get_rule(lang_ptr as CFStringRef);
                                    lang_str.to_string()
                                } else {
                                    "Unknown".to_string()
                                }
                            });
                            lang
                        } else {
                            "Unknown (Empty languages)".to_string()
                        }
                    } else {
                        let input_type_ptr = TISGetInputSourceProperty(
                            source,
                            CFString::new("TISPropertyInputSourceType").as_concrete_TypeRef(),
                        );

                        if !input_type_ptr.is_null() {
                            let input_type_cfstring: CFString =
                                TCFType::wrap_under_get_rule(input_type_ptr as CFStringRef);
                            format!("Unknown ({})", input_type_cfstring.to_string())
                        } else {
                            debug!("Localized name is null for source: {}", source_cfstring);
                            "Unknown".to_string()
                        }
                    }
                }
            };

            println!("{} - {}", source_cfstring, localized_name_str);
        }
    }
}

fn is_iswitch_running() -> bool {
    let output = match Command::new("pgrep")
        .arg("-f") // Search full command line
        .arg("iswitch")
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            error!("Failed to execute pgrep: {}", e);
            return false; // Assume not running in case of error
        }
    };

    // Convert output to string and count lines to determine the number of processes
    let output_str = String::from_utf8_lossy(&output.stdout);
    let process_count = output_str.lines().count();

    trace!("iswitch process count: {}", process_count);
    // If more than 1 process (current one + others), it's already running
    process_count > 1
}

// Add a helper function to safely get NSString values
unsafe fn safe_get_nsstring(obj: id) -> Option<String> {
    if obj.is_null() {
        return None;
    }

    let utf8_str: *const std::os::raw::c_char = msg_send![obj, UTF8String];
    if utf8_str.is_null() {
        return None;
    }

    Some(
        std::ffi::CStr::from_ptr(utf8_str)
            .to_string_lossy()
            .into_owned(),
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Set custom panic hook to log but not crash
    std::panic::set_hook(Box::new(|panic_info| {
        eprintln!("PANIC in iswitch application:");
        if let Some(location) = panic_info.location() {
            eprintln!(
                "  at file '{}' line {}: {}",
                location.file(),
                location.line(),
                panic_info
            );
        } else {
            eprintln!("  {}", panic_info);
        }
        // Log the panic but don't abort
    }));

    // Set initial app context
    set_app_context(Some("iswitch".to_string()));

    let args: Vec<String> = std::env::args().collect();

    // Handle -p option immediately with minimal output
    if args.len() > 1 && args[1] == "-p" {
        print_available_input_sources();
        return;
    }

    // Get config path and load config
    let config_path = get_config_path();
    let config = ensure_then_load(&config_path).await;

    // Set up logging system with proper error handling
    if let Err(e) = setup_logging(&config) {
        eprintln!("Failed to set up logging system: {}", e);
        // Continue without proper logging
    }

    info!("iSwitch starting, argument count: {}", args.len());

    if args.len() > 1 {
        match args[1].as_str() {
            "-s" => {
                if args.len() > 2 {
                    let desired_input_source = &args[2];
                    let current_input_source = get_current_input_source();

                    info!(
                        "Current input source: {}, Target input source: {}",
                        current_input_source, desired_input_source
                    );

                    if current_input_source != *desired_input_source {
                        info!(
                            "Switching input source from {} to {}",
                            current_input_source, desired_input_source
                        );

                        // Wrap the potentially panicking function in catch_unwind
                        if let Err(e) = std::panic::catch_unwind(|| {
                            switch_input_source(desired_input_source);
                        }) {
                            error!("Panic when switching input source: {:?}", e);
                        }
                    }

                    // Check if another instance is already running
                    if is_iswitch_running() {
                        info!("Another iswitch instance is already running. Exiting.");
                        return;
                    }
                } else {
                    error!("No input source specified. Usage: -s <input_source_id>");
                    return;
                }
            }
            "-h" | "--help" => {
                println!(
                    "iSwitch - Automatically switch input sources based on active application"
                );
                println!("\nUSAGE:");
                println!(
                    "  iswitch              Start in daemon mode and monitor application changes"
                );
                println!("  iswitch -s <input>   Switch to the specified input source");
                println!("  iswitch -p           Print all available input sources");
                println!("  iswitch -h           Show this help message");
                println!("\nCONFIGURATION:");
                println!("  Configuration file is located at ~/.config/iswitch/config.toml");
                return;
            }
            _ => {
                error!("Unknown option: {}. Use -h for help.", args[1]);
                return;
            }
        }
    }

    info!("Starting main application loop");
    let config = Arc::new(Mutex::new(config));
    let last_update_time = Arc::new(Mutex::new(Instant::now()));

    let (tx, mut rx) = channel(10);
    tokio::spawn(watch_config_for_changes(
        config_path.clone(),
        tx,
        last_update_time.clone(),
    ));

    unsafe {
        // Objc related code
        debug!("Initializing NSWorkspace");
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];

        if workspace.is_null() {
            error!("Failed to get shared workspace");
            return;
        }

        let notification_center: id = msg_send![workspace, notificationCenter];

        if notification_center.is_null() {
            error!("Failed to get notification center");
            return;
        }

        // Properly structured notification handler block without catch_unwind for config access
        let block = ConcreteBlock::new({
            let config = Arc::clone(&config);
            move |notification: id| {
                // Process notifications with proper error handling
                if notification.is_null() {
                    // Use eprintln for critical errors that might happen during logging failure
                    eprintln!("Received null notification");
                    return;
                }

                let app: id = msg_send![notification, userInfo];
                if app.is_null() {
                    eprintln!("Notification userInfo is null");
                    return;
                }

                let running_app: id = msg_send![app, objectForKey: NSString::alloc(nil).init_str("NSWorkspaceApplicationKey")];
                if running_app.is_null() {
                    eprintln!("Running application is null");
                    return;
                }

                // Use the safe helper to get application name
                let app_name = match safe_get_nsstring(msg_send![running_app, localizedName]) {
                    Some(name) => name,
                    None => {
                        eprintln!("Failed to get application name");
                        return;
                    }
                };

                // Set the app context for logging
                // Create a separate variable to avoid ownership issues
                let app_name_for_context = app_name.clone();
                set_app_context(Some(app_name_for_context));

                // Log safely with basic error printing as fallback
                match std::panic::catch_unwind(|| {
                    info!("Active application changed to: {}", app_name);
                }) {
                    Err(_) => eprintln!("Logging failed for application change: {}", app_name),
                    _ => {}
                }

                // Use a scoped copy of the config to minimize lock time
                // AVOID catch_unwind here due to RefUnwindSafe issues
                let app_map = {
                    // Minimize the time we hold the lock
                    let locked_config = config.lock();
                    locked_config.app.clone()
                };

                // Check if we have a configured input source for this app
                if let Some(input_source) = app_map.get(&app_name) {
                    // Get current input source safely
                    let current_input_source = get_current_input_source();

                    // Check if we need to switch input sources
                    if !current_input_source.contains(input_source) {
                        // Make a clone for the switch operation
                        let input_source_owned = input_source.clone();

                        // Log the switch operation safely
                        match std::panic::catch_unwind(|| {
                            info!(
                                "Switching to input source: {} for application: {}",
                                input_source, app_name
                            );
                        }) {
                            Err(_) => eprintln!("Logging failed for input source switch"),
                            _ => {}
                        }

                        // Perform the actual switch with panic protection
                        match std::panic::catch_unwind(|| {
                            switch_input_source(&input_source_owned);
                        }) {
                            Err(_) => eprintln!("Panic when switching input source"),
                            _ => {}
                        }
                    }
                }
            }
        });
        let block = &*block.copy();

        info!("Setting up application switch notification observer");

        let notification_name =
            NSString::alloc(nil).init_str("NSWorkspaceDidActivateApplicationNotification");
        if notification_name.is_null() {
            error!("Failed to create notification name string");
            return;
        }

        let _: id = msg_send![
            notification_center,
            addObserverForName: notification_name
            object: nil
            queue: nil
            usingBlock: block
        ];

        // Main run loop with safe error handling
        loop {
            // Handle config file changes
            if let Ok(()) = rx.try_recv() {
                info!("Config file change detected, reloading...");

                // Load new config outside the mutex lock
                let new_config = ensure_then_load(&config_path).await;

                // Get a mutex lock and update config
                let mut current_config = config.lock();

                if *current_config != new_config {
                    // If debug settings changed, reconfigure logging system
                    if current_config.debug != new_config.debug {
                        info!("Debug settings changed, reconfiguring logging system");
                        if let Err(e) = setup_logging(&new_config) {
                            error!("Failed to reconfigure logging system: {}", e);
                        }
                    }

                    *current_config = new_config;
                    info!("Configuration reloaded");

                    if log::log_enabled!(log::Level::Debug) {
                        debug!("New configuration: {:?}", *current_config);
                    }
                } else {
                    debug!("No changes in configuration, skipping reload");
                }
            }

            // Run one iteration of the main event loop with proper error handling
            let run_result = CFRunLoopRunInMode(
                CFString::new(K_CF_RUN_LOOP_DEFAULT_MODE).as_concrete_TypeRef(),
                0.5,
                false as u8,
            );

            // Check for run loop errors
            if run_result == 0 {
                // This is normal, just continue
            } else if run_result == 1 {
                // Run loop was stopped by another thread, which is usually not expected
                warn!("Run loop was stopped by another thread");
            } else {
                // Other errors, log with debug level as this is usually harmless
                debug!("Run loop returned unexpected code: {}", run_result);
            }
        }
    }
}
