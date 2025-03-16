#![allow(unexpected_cfgs)]

use block::ConcreteBlock;
use cocoa::base::{id, nil};
use cocoa::foundation::NSString;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{Boolean, CFTypeRef, TCFType, TCFTypeRef};
use core_foundation::runloop::CFRunLoopRunInMode;
use core_foundation::string::{CFString, CFStringRef};
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

// Custom log context that can be added to each log
thread_local! {
    static APP_CONTEXT: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
}

fn set_app_context(app_name: Option<String>) {
    APP_CONTEXT.with(|context| {
        *context.borrow_mut() = app_name;
    });
}

fn get_app_context() -> Option<String> {
    APP_CONTEXT.with(|context| context.borrow().clone())
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
    let home_dir = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    format!("{}/Library/Logs/iswitch.log", home_dir)
}

fn setup_logging(config: &AppConfig) -> Result<(), fern::InitError> {
    if !config.debug.enabled {
        // If debugging is not enabled, only configure basic console logging
        fern::Dispatch::new()
            .level(log::LevelFilter::Info)
            .chain(std::io::stdout())
            .apply()?;
        return Ok(());
    }

    // Parse log level
    let log_level = match config.debug.level.to_lowercase().as_str() {
        "trace" => log::LevelFilter::Trace,
        "debug" => log::LevelFilter::Debug,
        "info" => log::LevelFilter::Info,
        "warn" => log::LevelFilter::Warn,
        "error" => log::LevelFilter::Error,
        _ => log::LevelFilter::Info,
    };

    // Ensure log file directory exists
    let log_file = config
        .debug
        .file
        .replace("~", &std::env::var("HOME").unwrap_or_default());
    if let Some(parent) = Path::new(&log_file).parent() {
        std::fs::create_dir_all(parent).unwrap_or_else(|e| {
            eprintln!("Failed to create log directory: {}", e);
        });
    }

    // Configure log formatter with app context
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

    // Configure log dispatcher
    let dispatch = fern::Dispatch::new()
        .format(format_fn)
        .level(log_level)
        .chain(std::io::stdout()); // Always output to console

    // Add file output with the same format
    let file_config = fern::Dispatch::new()
        .format(format_fn)
        .chain(fern::log_file(&log_file)?);

    // Merge dispatchers and apply
    dispatch.chain(file_config).apply()?;

    info!(
        "Log system initialized, level: {}, log file: {}",
        config.debug.level, log_file
    );
    Ok(())
}

fn get_config_path() -> String {
    let config_path = if let Ok(config_dir) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(config_dir)
    } else if let Ok(home_dir) = std::env::var("HOME") {
        let mut path = PathBuf::from(home_dir);
        path.push(".config");
        path
    } else {
        panic!("Neither XDG_CONFIG_HOME nor HOME environment variables are set");
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
            fs::create_dir_all(parent)
                .await
                .expect("Failed to create config directory");
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
        fs::write(path, default_config)
            .await
            .expect("Failed to write default config file");
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
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            error!("Unable to read config file: {}", e);
            std::process::exit(1);
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

    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            if let Ok(Event {
                kind: EventKind::Modify(_),
                ..
            }) = res
            {
                let now = Instant::now();
                let mut last_update = last_update_time.lock().unwrap();

                if now.duration_since(*last_update) > debounce_duration {
                    *last_update = now;
                    if watcher_tx.try_send(()).is_err() {
                        error!("Failed to send file event update");
                    }
                }
            }
        },
        Config::default(),
    )
    .expect("Failed to create watcher");

    watcher
        .watch(Path::new(&config_path), RecursiveMode::NonRecursive)
        .expect("Failed to watch config file");

    info!("Started watching config file: {}", config_path);
    while let Some(()) = watcher_rx.recv().await {
        debug!("Config file change detected");
        tx.send(()).await.unwrap();
    }
}

fn print_available_input_sources() {
    unsafe {
        let source_list: CFArray =
            TCFType::wrap_under_get_rule(TISCreateInputSourceList(ptr::null(), 0));

        println!("Available input sources:");
        for i in 0..source_list.len() {
            let source = match source_list.get(i as isize) {
                Some(item) => item.as_void_ptr() as CFTypeRef,
                None => ptr::null(),
            };

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
                            let lang = languages.get(0).map_or("Unknown".to_string(), |item| {
                                let lang_ptr: *const c_void = item.as_void_ptr();
                                let lang_str: CFString =
                                    TCFType::wrap_under_get_rule(lang_ptr as CFStringRef);
                                lang_str.to_string()
                            });
                            lang
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
    let output = Command::new("pgrep")
        .arg("-f") // Search full command line
        .arg("iswitch")
        .output()
        .expect("Failed to execute pgrep");

    // Convert output to string and count lines to determine the number of processes
    let output_str = String::from_utf8_lossy(&output.stdout);
    let process_count = output_str.lines().count();

    trace!("iswitch process count: {}", process_count);
    // If more than 1 process (current one + others), it's already running
    process_count > 1
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Handle -p option immediately with minimal output
    if args.len() > 1 && args[1] == "-p" {
        print_available_input_sources();
        return;
    }

    // Get config path and load config
    let config_path = get_config_path();
    let config = ensure_then_load(&config_path).await;

    // Set up logging system
    if let Err(e) = setup_logging(&config) {
        eprintln!("Failed to set up logging system: {}", e);
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
                let app: id = msg_send![notification, userInfo];
                let running_app: id = msg_send![app, objectForKey: NSString::alloc(nil).init_str("NSWorkspaceApplicationKey")];
                let app_name: id = msg_send![running_app, localizedName];
                let app_name_str: *const std::os::raw::c_char = msg_send![app_name, UTF8String];

                let name = std::ffi::CStr::from_ptr(app_name_str)
                    .to_string_lossy()
                    .into_owned();

                // Set the app context for logging
                set_app_context(Some(name.clone()));

                info!("Active application changed to: {}", name);

                let config = Arc::clone(&config);
                tokio::spawn(async move {
                    // Set app context in the new thread too
                    set_app_context(Some(name.clone()));

                    let app_map = &config.lock().unwrap().app;
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
                let mut locked_config = config.lock().unwrap();
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
                    debug!("New configuration: {:?}", *locked_config);
                } else {
                    debug!("No changes in configuration, skipping reload");
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
