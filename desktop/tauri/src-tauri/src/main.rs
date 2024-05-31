// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::time::Duration;

use tauri::{AppHandle, Manager, RunEvent, WindowEvent};
use tauri_plugin_cli::CliExt;

// Library crates
mod portapi;
mod service;

#[cfg(target_os = "linux")]
mod xdg;

// App modules
mod portmaster;
mod traymenu;
mod window;

use log::{debug, error, info};
use portmaster::PortmasterExt;
use traymenu::setup_tray_menu;
use window::{close_splash_window, create_main_window};

#[macro_use]
extern crate lazy_static;

const FALLBACK_TO_OLD_UI_EXIT_CODE: i32 = -3;

#[derive(Clone, serde::Serialize)]
struct Payload {
    args: Vec<String>,
    cwd: String,
}

struct WsHandler {
    handle: AppHandle,
    background: bool,

    is_first_connect: bool,
}

impl portmaster::Handler for WsHandler {
    fn name(&self) -> String {
        "main-handler".to_string()
    }

    fn on_connect(&mut self, cli: portapi::client::PortAPI) -> () {
        info!("connection established, creating main window");

        // we successfully connected to Portmaster. Set is_first_connect to false
        // so we don't show the splash-screen when we loose connection.
        self.is_first_connect = false;

        if let Err(err) = close_splash_window(&self.handle) {
            error!("failed to close splash window: {}", err.to_string());
        }

        // create the main window now. It's not automatically visible by default.
        // Rather, the angular application will show the window itself when it finished
        // bootstrapping.
        if let Err(err) = create_main_window(&self.handle) {
            error!("failed to create main window: {}", err.to_string());
        } else {
            debug!("created main window")
        }

        let handle = self.handle.clone();
        tauri::async_runtime::spawn(async move {
            traymenu::tray_handler(cli, handle).await;
        });
    }

    fn on_disconnect(&mut self) {
        // if we're not running in background and this was the first connection attempt
        // then display the splash-screen.
        //
        // Once we had a successful connection the splash-screen will not be shown anymore
        // since there's already a main window with the angular application.
        if !self.background && self.is_first_connect {
            let _ = window::create_splash_window(&self.handle.clone());

            self.is_first_connect = false
        }
    }
}

fn show_webview_not_installed_dialog() -> i32 {
    use rfd::MessageDialog;

    let result = MessageDialog::new()
        .set_title("Portmaster")
        .set_description("Webkit is not installed. Please install it and run portmaster again")
        .set_buttons(rfd::MessageButtons::OkCancelCustom(
            "Go to install page".to_owned(),
            "Use old UI".to_owned(),
        ))
        .show();
    println!("{:?}", result);
    if let rfd::MessageDialogResult::Custom(result) = result {
        if result.eq("Go to install page") {
            _ = open::that("https://wiki.safing.io/en/Portmaster/Install/Webview");
            std::thread::sleep(Duration::from_secs(2));
            return 0;
        }
    }

    return FALLBACK_TO_OLD_UI_EXIT_CODE;
}

fn main() {
    if let Err(_) = tauri::webview_version() {
        std::process::exit(show_webview_not_installed_dialog());
    }

    let app = tauri::Builder::default()
        // Shell plugin for open_external support
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_log::Builder::default().build())
        // Clipboard support
        .plugin(tauri_plugin_clipboard_manager::init())
        // Dialog (Save/Open) support
        .plugin(tauri_plugin_dialog::init())
        // OS Version and Architecture support
        .plugin(tauri_plugin_os::init())
        // Single instance guard
        .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            let _ = app.emit("single-instance", Payload { args: argv, cwd });
        }))
        // Custom CLI arguments
        .plugin(tauri_plugin_cli::init())
        // Notification support
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            portmaster::commands::get_app_info,
            portmaster::commands::get_service_manager_status,
            portmaster::commands::start_service,
            portmaster::commands::get_state,
            portmaster::commands::set_state,
            portmaster::commands::should_show,
            portmaster::commands::should_handle_prompts
        ])
        // Setup the app an any listeners
        .setup(|app| {
            setup_tray_menu(app)?;
            portmaster::setup(app.handle().clone());

            // Setup the single-instance event listener that will create/focus the main window
            // or the splash-screen.
            let handle = app.handle().clone();
            app.listen_any("single-instance", move |_event| {
                let _ = window::open_window(&handle);
            });

            // Handle cli flags:
            //
            let mut background = false;
            match app.cli().matches() {
                Ok(matches) => {
                    debug!("cli matches={:?}", matches);

                    if let Some(bg_flag) = matches.args.get("background") {
                        match bg_flag.value.as_bool() {
                            Some(value) => {
                                background = value;
                                app.portmaster().set_show_after_bootstrap(!background);
                            }
                            None => {}
                        }
                    }

                    if let Some(nf_flag) = matches.args.get("with-notifications") {
                        match nf_flag.value.as_bool() {
                            Some(v) => {
                                app.portmaster().with_notification_support(v);
                            }
                            None => {}
                        }
                    }

                    if let Some(pf_flag) = matches.args.get("with-prompts") {
                        match pf_flag.value.as_bool() {
                            Some(v) => {
                                app.portmaster().with_connection_prompts(v);
                            }
                            None => {}
                        }
                    }
                }
                Err(err) => {
                    error!("failed to parse cli arguments: {}", err.to_string());
                }
            };

            // prepare a custom portmaster plugin handler that will show the splash-screen
            // (if not in --background) and launch the tray-icon handler.
            let handler = WsHandler {
                handle: app.handle().clone(),
                background,
                is_first_connect: true,
            };

            // register the custom handler
            app.portmaster().register_handler(handler);

            Ok(())
        })
        .any_thread()
        .build(tauri::generate_context!())
        .expect("error while running tauri application");

    app.run(|handle, e| match e {
        RunEvent::WindowEvent { label, event, .. } => {
            if label != "main" {
                // We only have one window at most so any other label is unexpected
                return;
            }

            // Do not let the user close the window, instead send an event to the main
            // window so we can show the "will not stop portmaster" dialog and let the window
            // close itself using
            //
            //    window.__TAURI__.window.getCurrent().close()
            //
            // Note: the above javascript does NOT trigger the CloseRequested event so
            // there's no need to handle that case here.
            //
            match event {
                WindowEvent::CloseRequested { api, .. } => {
                    debug!(
                        "window (label={}) close request received, forwarding to user-interface.",
                        label
                    );

                    api.prevent_close();
                    if let Some(window) = handle.get_webview_window(label.as_str()) {
                        let result = window.emit("exit-requested", "");
                        if let Err(err) = result {
                            error!("failed to emit event: {}", err.to_string());
                        }
                    } else {
                        error!("window was None");
                    }
                }
                _ => {}
            }
        }

        // TODO(vladimir): why was this needed?
        // RunEvent::ExitRequested { api, .. } => {
        //     api.prevent_exit();
        // }
        _ => {}
    });
}
