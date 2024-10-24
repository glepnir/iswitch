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
use std::ptr;
use std::sync::{Arc, Mutex};
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

#[derive(Debug, Deserialize, PartialEq)]
struct AppConfig {
    app: HashMap<String, String>,
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
    println!("Config path: {}, exist: {}", config_path, path.exists());

    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .expect("Failed to create config directory");
        }

        let default_config = r#"[app]"#;
        fs::write(path, default_config.trim())
            .await
            .expect("Failed to write default config file");
        println!("Created default config file at: {}", config_path);
    }

    match fs::read_to_string(config_path).await {
        Ok(config_content) => {
            println!("Config content read: {}", config_content);

            match toml::from_str::<AppConfig>(&config_content) {
                Ok(parsed_config) => parsed_config,
                Err(e) => {
                    eprintln!("Failed to parse config file: {}", e);
                    eprintln!("Config content: {}", config_content);
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("Unable to read config file: {}", e);
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
                #[cfg(debug_assertions)]
                {
                    println!("Current input source ID: {}", source_id);
                }
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
                    TISSelectInputSource(source);
                    break;
                }
            }
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
                        eprintln!("Failed to send file event update");
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

    while let Some(()) = watcher_rx.recv().await {
        tx.send(()).await.unwrap();
    }
}

fn print_available_input_sources() {
    unsafe {
        // TODO: clean up move to a single function that i can reuse in switch_input_source
        let source_list: CFArray =
            TCFType::wrap_under_get_rule(TISCreateInputSourceList(ptr::null(), 0));

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
                                println!("Localized name is null for source: {}", source_cfstring);
                                "Unknown".to_string()
                            }
                        }
                    }
                };

                println!("{}, {}", source_cfstring, localized_name_str);
            }
        }
    }
}

fn is_iswitch_running() -> bool {
    let output = std::process::Command::new("pgrep")
        .arg("iswitch")
        .output()
        .expect("Failed to execute pgrep");

    !output.stdout.is_empty()
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        match args[1].as_str() {
            "-s" => {
                if args.len() > 2 {
                    let desired_input_source = &args[2];
                    let current_input_source = get_current_input_source();
                    if current_input_source != *desired_input_source {
                        #[cfg(debug_assertions)]
                        {
                            println!(
                                "Switching input source from {} to {}",
                                current_input_source, desired_input_source
                            );
                        }
                        switch_input_source(desired_input_source);
                    }

                    // already have a iswitch progress
                    if is_iswitch_running() {
                        return;
                    }
                } else {
                    eprintln!("No input source specified. Usage: -s <input_source_id>");
                }
            }
            "-p" => {
                print_available_input_sources();
                return;
            }
            _ => {
                eprintln!(
                    "Unknown option: {}. Usage: -s <input_source_id> or -p",
                    args[1]
                );
                return;
            }
        }
    }

    let config_path = get_config_path();
    let config = Arc::new(Mutex::new(ensure_then_load(&config_path).await));
    let last_update_time = Arc::new(Mutex::new(Instant::now()));

    let (tx, mut rx) = channel(10); // buffer is enough?
    tokio::spawn(watch_config_for_changes(
        config_path.clone(),
        tx,
        last_update_time.clone(),
    ));

    unsafe {
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
                #[cfg(debug_assertions)]
                {
                    println!("Current app {}", name);
                }

                let config = Arc::clone(&config);
                tokio::spawn(async move {
                    if let Some(input_source) = config.lock().unwrap().app.get(&name) {
                        let current_input_source = get_current_input_source();
                        if !current_input_source.contains(input_source) {
                            println!("Switching to input: {}", input_source);
                            switch_input_source(input_source);
                        }
                    }
                });
            }
        });

        let block = &*block.copy();

        let _: id = msg_send![
            notification_center,
            addObserverForName: NSString::alloc(nil).init_str("NSWorkspaceDidActivateApplicationNotification")
            object: nil
            queue: nil
            usingBlock: block
        ];

        loop {
            if let Ok(()) = rx.try_recv() {
                let new_config = ensure_then_load(&config_path).await;
                let mut locked_config = config.lock().unwrap();
                if *locked_config != new_config {
                    *locked_config = new_config;
                    println!("Config reloaded: {:?}", *locked_config);
                } else {
                    println!("No changes in config, skipping reload.");
                }
            }

            CFRunLoopRunInMode(
                CFString::new(K_CF_RUN_LOOP_DEFAULT_MODE).as_concrete_TypeRef(),
                1.0,
                false as u8,
            );
        }
    }
}
