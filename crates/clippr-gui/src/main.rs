//! clippr-gui: the clip library, settings and editor window, plus the quick overlay.
//!
//! Neither mode stays resident. The daemon launches `clippr-gui --overlay` on the overlay
//! hotkey and terminates it on the next press.

mod daemon;
mod editor;
mod library;
mod overlay;
mod settings;
mod state;
mod timeline;
mod util;
mod window;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

const APP_ID: &str = "dev.clippr.Clippr";
const OVERLAY_APP_ID: &str = "dev.clippr.Clippr.Overlay";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Library,
    Settings,
    Overlay,
}

fn parse_mode(args: &[std::ffi::OsString]) -> Option<Mode> {
    let mut mode = Mode::Library;
    for arg in args.iter().skip(1) {
        match arg.to_str() {
            Some("--overlay") => mode = Mode::Overlay,
            Some("--settings") => mode = Mode::Settings,
            Some("--version") => {
                println!("clippr-gui {}", clippr_core::VERSION);
                return None;
            }
            Some("-h" | "--help") => {
                println!("usage: clippr-gui [--overlay | --settings]");
                return None;
            }
            _ => {
                eprintln!("clippr-gui: unknown argument {arg:?}");
                return None;
            }
        }
    }
    Some(mode)
}

fn main() -> glib::ExitCode {
    let args: Vec<_> = std::env::args_os().collect();
    let Some(mode) = parse_mode(&args) else {
        return glib::ExitCode::SUCCESS;
    };

    let app_id = if mode == Mode::Overlay { OVERLAY_APP_ID } else { APP_ID };
    let app = adw::Application::builder()
        .application_id(app_id)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    app.connect_startup(|_| load_css());

    // A second launch is forwarded to the running instance, which reuses its window.
    app.connect_command_line(|app, cmdline| {
        let mode = parse_mode(&cmdline.arguments()).unwrap_or(Mode::Library);
        present(app, mode);
        glib::ExitCode::SUCCESS
    });

    let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    app.run_with_args(&args)
}

fn present(app: &adw::Application, mode: Mode) {
    if let Some(win) = app.active_window() {
        if mode == Mode::Settings {
            if let Some(win) = win.downcast_ref::<adw::ApplicationWindow>() {
                window::show_settings(win);
            }
        }
        win.present();
        return;
    }

    let state = state::State::new();
    match mode {
        Mode::Overlay => overlay::build(app, &state).present(),
        Mode::Library | Mode::Settings => {
            let win = window::build(app, &state);
            if mode == Mode::Settings {
                window::show_settings(&win);
            }
            win.present();
        }
    }
    state.start();

    #[cfg(debug_assertions)]
    debug_snapshot::schedule(app);
}

/// Debug builds only: `CLIPPR_GUI_SNAPSHOT=out.png` renders the active window to a PNG after
/// a short delay, and `CLIPPR_GUI_OPEN_CLIP=<id>` opens that clip in the editor first.
/// Used to check layouts on compositors that don't allow per-window screenshots.
#[cfg(debug_assertions)]
mod debug_snapshot {
    use super::*;

    pub fn schedule(app: &adw::Application) {
        let Ok(path) = std::env::var("CLIPPR_GUI_SNAPSHOT") else { return };
        let app = app.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(2500), move || {
            let Some(win) = app.active_window() else { return };
            let (w, h) = (win.width() as f64, win.height() as f64);
            let paintable = gtk::WidgetPaintable::new(Some(&win));
            let snapshot = gtk::Snapshot::new();
            paintable.snapshot(&snapshot, w, h);
            let Some(node) = snapshot.to_node() else { return };
            let Some(renderer) = win.renderer() else { return };
            let texture = renderer.render_texture(&node, None);
            if let Err(e) = texture.save_to_png(&path) {
                eprintln!("snapshot failed: {e}");
            }
            eprintln!("snapshot written to {path}");
        });
    }
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
