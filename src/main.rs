use block::ConcreteBlock;
use cocoa::base::{id, nil};
use cocoa::foundation::NSString;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{Boolean, CFTypeRef, TCFType, TCFTypeRef};
use core_foundation::runloop::CFRunLoopRun;
use core_foundation::string::{CFString, CFStringRef};
use objc::{class, msg_send, sel, sel_impl};
use std::os::raw::c_void;
use std::ptr;

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

fn switch_to_abc() {
    let abc_input_source_id = CFString::new("com.apple.keylayout.ABC");
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

                if source_cfstring == abc_input_source_id {
                    TISSelectInputSource(source);
                    break;
                }
            }
        }
    }
}

fn is_terminal(app_name: &str) -> bool {
    app_name == "Alacritty"
}

fn is_iswitch_running() -> bool {
    let output = std::process::Command::new("pgrep")
        .arg("iswitch")
        .output()
        .expect("Failed to execute pgrep");

    !output.stdout.is_empty()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 && args[1] == "--check-and-switch" {
        let current_input_source = get_current_input_source();
        if current_input_source != "com.apple.keylayout.ABC" {
            switch_to_abc();
        }
        if is_iswitch_running() {
            return;
        }
    }

    unsafe {
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let notification_center: id = msg_send![workspace, notificationCenter];

        let block = ConcreteBlock::new(|notification: id| {
            let app: id = msg_send![notification, userInfo];
            let running_app: id = msg_send![app, objectForKey: NSString::alloc(nil).init_str("NSWorkspaceApplicationKey")];
            let app_name: id = msg_send![running_app, localizedName];
            let app_name_str: *const std::os::raw::c_char = msg_send![app_name, UTF8String];

            let name = std::ffi::CStr::from_ptr(app_name_str)
                .to_string_lossy()
                .into_owned();

            #[cfg(debug_assertions)]
            {
                println!("App activated: {}", name);
            }

            if is_terminal(&name) {
                let current_input_source = get_current_input_source();
                #[cfg(debug_assertions)]
                {
                    println!("current input source: {}", current_input_source,);
                }

                if !current_input_source.contains("keylayout.ABC") {
                    #[cfg(debug_assertions)]
                    {
                        println!("try switch to abc");
                    }
                    switch_to_abc();
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

        CFRunLoopRun();
    }
}
