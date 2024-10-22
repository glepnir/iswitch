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
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, RwLock};
use std::thread;
use std::{
    collections::HashMap,
    fs,
    time::{Duration, Instant},
};

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

fn ensure_then_load(config_path: &str) -> AppConfig {
    let path = Path::new(config_path);
    println!("Config path: {}, exist: {}", config_path, path.exists());

    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("Failed to create config directory");
        }

        let default_config = r#"
[app]
Alacritty = "com.apple.keylayout.ABC"
WeChat = "com.apple.inputmethod.SCIM.ITABC"
QQ = "com.apple.inputmethod.SCIM.ITABC"
"#;

        fs::write(path, default_config.trim()).expect("Failed to write default config file");
        println!("Created default config file at: {}", config_path);
    }

    match fs::read_to_string(config_path) {
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

fn watch_config_for_changes(config_path: String, tx: Sender<()>) {
    let last_update_time = Arc::new(RwLock::new(Instant::now()));

    thread::spawn({
        let last_update_time = Arc::clone(&last_update_time);
        move || {
            let mut watcher = RecommendedWatcher::new(
                move |res: notify::Result<Event>| {
                    if let Ok(event) = res {
                        if matches!(event.kind, EventKind::Modify(_)) {
                            let now = Instant::now();
                            let mut last_update = last_update_time.write().unwrap();

                            if now.duration_since(*last_update) > Duration::from_millis(500) {
                                *last_update = now;
                                println!("Config file changed, reloading...");
                                tx.send(()).unwrap();
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

            loop {
                std::thread::park();
            }
        }
    });
}

fn main() {
    let config_path = get_config_path();
    let config = Arc::new(RwLock::new(ensure_then_load(&config_path)));

    let (tx, rx) = channel();
    watch_config_for_changes(config_path.clone(), tx);

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

                if let Ok(current_config) = config.read() {
                    if let Some(input_source) = current_config.app.get(&name) {
                        let current_input_source = get_current_input_source();
                        if !current_input_source.contains(input_source) {
                            #[cfg(debug_assertions)]
                            {
                                println!("Switching to input: {}", input_source);
                            }
                            switch_input_source(input_source);
                        }
                    }
                }
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
                let new_config = ensure_then_load(&config_path);
                if let Ok(mut locked_config) = config.write() {
                    if *locked_config != new_config {
                        *locked_config = new_config;
                        println!("Config reloaded: {:?}", *locked_config);
                    } else {
                        println!("No changes in config, skipping reload.");
                    }
                }
            }

            CFRunLoopRunInMode(
                CFString::new(K_CF_RUN_LOOP_DEFAULT_MODE).as_concrete_TypeRef(),
                0.1,
                false as u8,
            );
        }
    }
}
