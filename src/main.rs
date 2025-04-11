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
use serde::Deserialize;
use std::collections::HashMap;
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr;
use std::sync::{Arc, Mutex};
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

fn set_app_context(app_name: Option<String>) {
    if let Ok(mut context) = APP_CONTEXT.lock() {
        *context = app_name;
    } else {
        // Handle potential poisoned mutex to avoid panics
        eprintln!("Warning: Failed to set app context - mutex was poisoned");
    }
}

fn get_app_context() -> Option<String> {
    match APP_CONTEXT.lock() {
        Ok(context) => context.clone(),
        Err(_) => {
            // Handle potential poisoned mutex to avoid panics
            eprintln!("Warning: Failed to get app context - mutex was poisoned");
            Some("iswitch".to_string()) // Fallback to default
        }
    }
}

#[derive(Debug, Deserialize, PartialEq)]
struct AppConfig {
    app: HashMap<String, String>,
    #[serde(default)]
    debug: DebugConfig,
}

#[derive(Debug, Deserialize, PartialEq)]
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

// Setup console-only logging as a fallback in case of errors
fn setup_console_only_logging() -> Result<(), fern::InitError> {
    fern::Dispatch::new()
        .level(log::LevelFilter::Info)
        .chain(std::io::stdout())
        .apply()?;

    eprintln!("Initialized console-only logging due to file logging setup failure");
    Ok(())
}

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
    let log_file = config
        .debug
        .file
        .replace("~", &std::env::var("HOME").unwrap_or_default());

    if let Some(parent) = Path::new(&log_file).parent() {
        if !parent.exists() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("Failed to create log directory: {}", e);
                return setup_console_only_logging();
            }
        }
    }

    // Configure log formatter with app context and error handling
    let format_fn =
        |out: fern::FormatCallback, message: &std::fmt::Arguments, record: &log::Record| {
            let app_context = get_app_context().unwrap_or_else(|| "iswitch".to_string());
            out.finish(format_args!(
                "{} [{}] [{}] [{}] {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                record.level(),
                record.target(),
                app_context,
                message
            ))
        };

    // Create file with appropriate error handling
    let file_result = match fern::log_file(&log_file) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("Failed to open log file {}: {}", log_file, e);
            return setup_console_only_logging();
        }
    };

    // Configure log dispatcher
    let dispatch = fern::Dispatch::new()
        .format(format_fn)
        .level(log_level)
        .chain(std::io::stdout()); // Always output to console

    // Add file output with the same format
    let file_config = fern::Dispatch::new().format(format_fn).chain(file_result);

    // Merge dispatchers and apply
    match dispatch.chain(file_config).apply() {
        Ok(_) => {
            info!(
                "Log system initialized, level: {}, log file: {}",
                config.debug.level, log_file
            );
            Ok(())
        }
        Err(e) => {
            eprintln!("Failed to initialize logging system: {}", e);
            setup_console_only_logging()
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

    // Only print config diagnostics in debug mode
    if log::log_enabled!(log::Level::Debug) {
        debug!("Config path: {}, exist: {}", config_path, path.exists());
    }

    if !path.exists() {
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent).await {
                error!("Failed to create config directory: {}", e);
                // Return a default config instead of panicking
                return AppConfig {
                    app: HashMap::new(),
                    debug: DebugConfig::default(),
                };
            }
        }

        // Richer default configuration, including debug section
        let default_config = r#"[app]
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
        if let Err(e) = fs::write(path, default_config).await {
            error!("Failed to write default config file: {}", e);
            // Return a default config instead of panicking
            return AppConfig {
                app: HashMap::new(),
                debug: DebugConfig::default(),
            };
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
                    AppConfig {
                        app: HashMap::new(),
                        debug: DebugConfig::default(),
                    }
                }
            }
        }
        Err(e) => {
            error!("Unable to read config file: {}", e);
            // Return a default config instead of panicking
            AppConfig {
                app: HashMap::new(),
                debug: DebugConfig::default(),
            }
        }
    }
}

fn get_current_input_source() -> String {
    unsafe {
        let current_source = TISCopyCurrentKeyboardInputSource();
        if !current_source.is_null() {
            let source_name = TISGetInputSourceProperty(
                current_source,
                CFString::new(K_TIS_PROPERTY_INPUT_SOURCE_ID).as_concrete_TypeRef(),
            );
            if !source_name.is_null() {
                let cf_str: CFString = TCFType::wrap_under_get_rule(source_name as CFStringRef);
                let source_id = cf_str.to_string();
                debug!("Current input source ID: {}", source_id);
                return source_id;
            }
        }
        String::from("Unknown")
    }
}

fn switch_input_source(input_source_id: &str) {
    let input_source_cfstring = CFString::new(input_source_id);
    unsafe {
        let source_list: CFArray =
            TCFType::wrap_under_get_rule(TISCreateInputSourceList(ptr::null(), 0));

        info!("Attempting to switch to input source: {}", input_source_id);

        let mut found = false;
        for i in 0..source_list.len() {
            let source = match source_list.get(i as isize) {
                Some(item) => item.as_void_ptr() as CFTypeRef,
                None => ptr::null(),
            };

            if source.is_null() {
                continue; // Skip null sources safely
            }

            let source_id = TISGetInputSourceProperty(
                source as CFTypeRef,
                CFString::new(K_TIS_PROPERTY_INPUT_SOURCE_ID).as_concrete_TypeRef(),
            );

            if !source_id.is_null() {
                let source_cfstring: CFString =
                    TCFType::wrap_under_get_rule(source_id as CFStringRef);

                if source_cfstring == input_source_cfstring {
                    info!("Found matching input source, switching...");
                    TISSelectInputSource(source);
                    found = true;
                    break;
                }
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
                // Safe mutex locking with error handling
                let now = Instant::now();
                match last_update_time.lock() {
                    Ok(mut last_update) => {
                        if now.duration_since(*last_update) > debounce_duration {
                            *last_update = now;
                            if watcher_tx.try_send(()).is_err() {
                                // This is normal when shutting down, so debug level is appropriate
                                debug!("Failed to send file event update - channel may be closed");
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to lock last_update_time: {}", e);
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
        let source_list: CFArray =
            TCFType::wrap_under_get_rule(TISCreateInputSourceList(ptr::null(), 0));

        println!("Available input sources:");
        for i in 0..source_list.len() {
            let source = match source_list.get(i as isize) {
                Some(item) => item.as_void_ptr() as CFTypeRef,
                None => continue, // Skip invalid items safely
            };

            if source.is_null() {
                continue; // Skip null sources safely
            }

            let source_id = TISGetInputSourceProperty(
                source as CFTypeRef,
                CFString::new(K_TIS_PROPERTY_INPUT_SOURCE_ID).as_concrete_TypeRef(),
            );

            if !source_id.is_null() {
                let source_cfstring: CFString =
                    TCFType::wrap_under_get_rule(source_id as CFStringRef);

                let localized_name = TISGetInputSourceProperty(
                    source,
                    CFString::new("TISPropertyLocalizedName").as_concrete_TypeRef(),
                );
                let localized_name_str = if !localized_name.is_null() {
                    let localized_cfstring: CFString =
                        TCFType::wrap_under_get_rule(localized_name as CFStringRef);
                    localized_cfstring.to_string()
                } else {
                    let input_mode_id = TISGetInputSourceProperty(
                        source,
                        CFString::new("TISPropertyInputModeID").as_concrete_TypeRef(),
                    );

                    if !input_mode_id.is_null() {
                        let input_mode_cfstring: CFString =
                            TCFType::wrap_under_get_rule(input_mode_id as CFStringRef);
                        input_mode_cfstring.to_string()
                    } else {
                        let input_languages = TISGetInputSourceProperty(
                            source,
                            CFString::new("TISPropertyInputSourceLanguages").as_concrete_TypeRef(),
                        );

                        if !input_languages.is_null() {
                            let languages: CFArray =
                                TCFType::wrap_under_get_rule(input_languages as CFArrayRef);

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
                            let input_type = TISGetInputSourceProperty(
                                source,
                                CFString::new("TISPropertyInputSourceType").as_concrete_TypeRef(),
                            );

                            if !input_type.is_null() {
                                let input_type_cfstring: CFString =
                                    TCFType::wrap_under_get_rule(input_type as CFStringRef);
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

#[tokio::main]
async fn main() {
    // Set custom panic hook to prevent crashes
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
    }));

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
                        switch_input_source(desired_input_source);
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
        let notification_center: id = msg_send![workspace, notificationCenter];

        let block = ConcreteBlock::new({
            let config = Arc::clone(&config);
            move |notification: id| {
                // Wrap the notification handler in a catch_unwind to prevent crashes
                if let Err(e) = std::panic::catch_unwind(|| {
                    let app: id = msg_send![notification, userInfo];
                    let running_app: id = msg_send![app, objectForKey: NSString::alloc(nil).init_str("NSWorkspaceApplicationKey")];
                    let app_name: id = msg_send![running_app, localizedName];
                    let app_name_str: *const std::os::raw::c_char = msg_send![app_name, UTF8String];

                    // Check for null pointer before dereferencing
                    if app_name_str.is_null() {
                        error!("Application name is null");
                        return;
                    }

                    let name = std::ffi::CStr::from_ptr(app_name_str)
                        .to_string_lossy()
                        .into_owned();

                    // Set the app context for logging
                    set_app_context(Some(name.clone()));

                    info!("Active application changed to: {}", name);

                    let config = Arc::clone(&config);
                    tokio::spawn(async move {
                        // Wrap the task body in catch_unwind to prevent crashes
                        if let Err(e) = std::panic::catch_unwind(|| {
                            // Set app context in the new thread too
                            set_app_context(Some(name.clone()));
                        }) {
                            error!("Panic in tokio task when setting app context: {:?}", e);
                        }

                        // Lock config with error handling
                        let app_map = match config.lock() {
                            Ok(guard) => guard.app.clone(), // Clone to minimize lock time
                            Err(e) => {
                                error!("Failed to lock config: {}", e);
                                return;
                            }
                        };

                        if let Some(input_source) = app_map.get(&name) {
                            let current_input_source = get_current_input_source();
                            if !current_input_source.contains(input_source) {
                                info!(
                                    "Switching to input source: {} for application: {}",
                                    input_source, name
                                );
                                switch_input_source(input_source);
                            } else {
                                debug!(
                                    "Already using correct input source: {}",
                                    current_input_source
                                );
                            }
                        } else {
                            debug!("No input source configured for application: {}", name);
                        }
                    });
                }) {
                    error!("Panic in notification handler: {:?}", e);
                }
            }
        });

        let block = &*block.copy();

        info!("Setting up application switch notification observer");
        let _: id = msg_send![
            notification_center,
            addObserverForName: NSString::alloc(nil).init_str("NSWorkspaceDidActivateApplicationNotification")
            object: nil
            queue: nil
            usingBlock: block
        ];

        loop {
            if let Ok(()) = rx.try_recv() {
                info!("Config file change detected, reloading...");
                let new_config = ensure_then_load(&config_path).await;

                // Lock config with error handling
                match config.lock() {
                    Ok(mut locked_config) => {
                        if *locked_config != new_config {
                            // If debug settings changed, reconfigure logging system
                            if locked_config.debug != new_config.debug {
                                info!("Debug settings changed, reconfiguring logging system");
                                if let Err(e) = setup_logging(&new_config) {
                                    error!("Failed to reconfigure logging system: {}", e);
                                }
                            }

                            *locked_config = new_config;
                            info!("Configuration reloaded");

                            if log::log_enabled!(log::Level::Debug) {
                                debug!("New configuration: {:?}", *locked_config);
                            }
                        } else {
                            debug!("No changes in configuration, skipping reload");
                        }
                    }
                    Err(e) => {
                        error!("Failed to lock config for reloading: {}", e);
                    }
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
