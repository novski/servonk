/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! The `servo` test application.
//!
//! Creates a `Servo` instance with a simple implementation of
//! the compositor's `WindowMethods` to create a working web browser.
//!
//! This browser's implementation of `WindowMethods` is built on top
//! of [glutin], the cross-platform OpenGL utility and windowing
//! library.
//!
//! For the engine itself look next door in `components/servo/lib.rs`.
//!
//! [glutin]: https://github.com/tomaka/glutin

#![cfg_attr(feature = "unstable", feature(core_intrinsics))]

#[cfg(target_os = "android")]
extern crate android_injected_glue;
extern crate api_server;
extern crate backtrace;
#[macro_use]
extern crate bitflags;
extern crate euclid;
#[cfg(target_os = "windows")]
extern crate gdi32;
extern crate gleam;
extern crate glutin;
// The window backed by glutin
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate log;
#[cfg(any(target_os = "linux", target_os = "macos"))]
extern crate osmesa_sys;
extern crate servo;
#[cfg(all(feature = "unstable", not(target_os = "android")))]
#[macro_use]
extern crate sig;
extern crate tinyfiledialogs;
#[cfg(target_os = "windows")]
extern crate user32;
#[cfg(target_os = "windows")]
extern crate winapi;
extern crate winit;

mod glutin_app;
mod resources;

use api_server::server::MessageToSystemApp;
use backtrace::Backtrace;
use servo::Servo;
use servo::compositing::windowing::{WindowEvent, WindowMethods};

#[cfg(target_os = "android")]
use servo::config;
use servo::config::opts::{self, parse_url_or_filename, ArgumentParsingResult};
use servo::config::servo_version;
use servo::ipc_channel::ipc;
use servo::servo_config::prefs::PREFS;
use servo::servo_url::ServoUrl;
use std::env;
use std::panic;
use std::process;
use std::sync::mpsc;
use std::thread;

mod browser;

pub mod platform {
    #[cfg(target_os = "macos")]
    pub use platform::macos::deinit;

    #[cfg(target_os = "macos")]
    pub mod macos;

    #[cfg(not(target_os = "macos"))]
    pub fn deinit() {}
}

#[cfg(all(feature = "unstable", not(target_os = "android")))]
fn install_crash_handler() {
    use backtrace::Backtrace;
    use sig::ffi::Sig;
    use std::intrinsics::abort;
    use std::thread;

    fn handler(_sig: i32) {
        let name = thread::current()
            .name()
            .map(|n| format!(" for thread \"{}\"", n))
            .unwrap_or("".to_owned());
        println!("Stack trace{}\n{:?}", name, Backtrace::new());
        unsafe {
            // N.B. Using process::abort() here causes the crash handler to be
            //      triggered recursively.
            abort();
        }
    }

    signal!(Sig::SEGV, handler); // handle segfaults
    signal!(Sig::ILL, handler); // handle stack overflow and unsupported CPUs
    signal!(Sig::IOT, handler); // handle double panics
    signal!(Sig::BUS, handler); // handle invalid memory access
}

#[cfg(any(not(feature = "unstable"), target_os = "android"))]
fn install_crash_handler() {}

fn main() {
    install_crash_handler();

    resources::init();

    // Parse the command line options and store them globally
    let opts_result = opts::from_cmdline_args(&*args());

    let content_process_token = if let ArgumentParsingResult::ContentProcess(token) = opts_result {
        Some(token)
    } else {
        if opts::get().is_running_problem_test && env::var("RUST_LOG").is_err() {
            env::set_var("RUST_LOG", "compositing::constellation");
        }

        None
    };

    // TODO: once log-panics is released, can this be replaced by
    // log_panics::init()?
    panic::set_hook(Box::new(|info| {
        warn!("Panic hook called.");
        let msg = match info.payload().downcast_ref::<&'static str>() {
            Some(s) => *s,
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => &**s,
                None => "Box<Any>",
            },
        };
        let current_thread = thread::current();
        let name = current_thread.name().unwrap_or("<unnamed>");
        if let Some(location) = info.location() {
            println!(
                "{} (thread {}, at {}:{})",
                msg,
                name,
                location.file(),
                location.line()
            );
        } else {
            println!("{} (thread {})", msg, name);
        }
        if env::var("RUST_BACKTRACE").is_ok() {
            println!("{:?}", Backtrace::new());
        }

        error!("{}", msg);
    }));

    setup_logging();

    if let Some(token) = content_process_token {
        return servo::run_content_process(token);
    }

    if opts::get().is_printing_version {
        println!("{}", servo_version());
        process::exit(0);
    }

    let (ws_sender, ws_receiver) = mpsc::channel();
    api_server::start_api_server(ws_sender);
    let api_server = ws_receiver
        .recv()
        .expect("Failed to get the api server address");

    let window = glutin_app::create_window();

    let mut browser = browser::Browser::new(window.clone());

    // If the url is not provided, we fallback to the homepage in PREFS,
    // or a blank page in case the homepage is not set either.
    let cwd = env::current_dir().unwrap();
    let cmdline_url = opts::get().url.clone();
    let pref_url = PREFS
        .get("shell.homepage")
        .as_string()
        .and_then(|str| parse_url_or_filename(&cwd, str).ok());
    let blank_url = ServoUrl::parse("about:blank").ok();

    let target_url = cmdline_url.or(pref_url).or(blank_url).unwrap();

    let mut servo = Servo::new(window.clone());

    let (sender, receiver) = ipc::channel().unwrap();
    servo.handle_events(vec![WindowEvent::NewBrowser(target_url, sender)]);
    let browser_id = receiver.recv().unwrap();
    browser.set_browser_id(browser_id);
    servo.handle_events(vec![WindowEvent::SelectBrowser(browser_id)]);

    servo.setup_logging();

    let waker = window.create_event_loop_waker();
    let (sender, api_receiver) = mpsc::channel();
    api_server.do_send(api_server::server::SetServoSender { waker, sender });

    window.run(|| {
        let win_events = window.get_events();

        // FIXME: this could be handled by Servo. We don't need
        // a repaint_synchronously function exposed.
        let need_resize = win_events.iter().any(|e| match *e {
            WindowEvent::Resize => true,
            _ => false,
        });

        // Check if we have a `Idle` event and in this case
        // drain any message from the web socket server.
        win_events.iter().for_each(|event| {
            match *event {
                WindowEvent::Idle => {
                    let mut events = Vec::new();
                    while let Ok(msg) = api_receiver.try_recv() {
                        debug!("Frontend message: {:?}", msg);
                        // Build a Servo event and queue it.
                        events.push(msg.into());
                    }
                    if !events.is_empty() {
                        servo.handle_events(events);
                    }
                }
                _ => {}
            }
        });

        browser.handle_window_events(win_events);

        let mut servo_events = servo.get_events();
        loop {
            // Filter the events that will be sent out to the system app.
            let mut final_servo_events = vec![];
            for event in servo_events {
                if let Some(system_event) = MessageToSystemApp::from_embedder_msg(&event) {
                    api_server.do_send(system_event);
                } else {
                    final_servo_events.push(event);
                }
            }

            browser.handle_servo_events(final_servo_events);
            servo.handle_events(browser.get_events());
            if browser.shutdown_requested() {
                return true;
            }
            servo_events = servo.get_events();
            if servo_events.is_empty() {
                break;
            }
        }

        if need_resize {
            servo.repaint_synchronously();
        }
        false
    });

    servo.deinit();

    platform::deinit()
}

#[cfg(target_os = "android")]
fn setup_logging() {
    // Piping logs from stdout/stderr to logcat happens in android_injected_glue.
    env::set_var("RUST_LOG", "error");

    unsafe { android_injected_glue::ffi::app_dummy() };
}

#[cfg(not(target_os = "android"))]
fn setup_logging() {}

#[cfg(target_os = "android")]
/// Attempt to read parameters from a file since they are not passed to us in Android environments.
/// The first line should be the "servo" argument and the last should be the URL to load.
/// Blank lines and those beginning with a '#' are ignored.
/// Each line should be a separate parameter as would be parsed by the shell.
/// For example, "servo -p 10 http://en.wikipedia.org/wiki/Rust" would take 4 lines.
fn args() -> Vec<String> {
    use std::error::Error;
    use std::fs::File;
    use std::io::{BufRead, BufReader};

    let mut params_file = config::basedir::default_config_dir();
    params_file.push("android_params");
    match File::open(params_file.to_str().unwrap()) {
        Ok(f) => {
            let mut vec = Vec::new();
            let file = BufReader::new(&f);
            for line in file.lines() {
                let l = line.unwrap().trim().to_owned();
                // ignore blank lines and those that start with a '#'
                match l.is_empty() || l.as_bytes()[0] == b'#' {
                    true => (),
                    false => vec.push(l),
                }
            }
            vec
        }
        Err(e) => {
            debug!(
                "Failed to open params file '{}': {}",
                params_file.to_str().unwrap(),
                Error::description(&e)
            );
            vec![
                "servo".to_owned(),
                "http://en.wikipedia.org/wiki/Rust".to_owned(),
            ]
        }
    }
}

#[cfg(not(target_os = "android"))]
fn args() -> Vec<String> {
    env::args().collect()
}

#[cfg(target_os = "android")]
#[no_mangle]
#[inline(never)]
#[allow(non_snake_case)]
pub extern "C" fn android_main(app: *mut ()) {
    android_injected_glue::android_main2(app as *mut _, move |_, _| main());
}
