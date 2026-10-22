//! Quick overlay: a small panel for clipping, pausing and the settings people flip mid-game.
//! It lives only while shown. Escape or the overlay hotkey closes the process.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use clippr_core::clip::ClipMeta;
use clippr_core::config::Quality;
use clippr_core::ipc::CaptureState;
use gtk::glib;

use crate::state::State;
use crate::util;

const QUALITIES: [(Quality, &str); 3] = [(Quality::Low, "Low"), (Quality::Medium, "Medium"), (Quality::High, "High")];

pub fn build(app: &adw::Application, state: &Rc<State>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Clippr")
        .default_width(380)
        .resizable(false)
        .build();
    window.add_css_class("clippr-overlay");

    // Header: state and close
    let dot = util::status_dot();
    let state_label = gtk::Label::builder().xalign(0.0).build();
    state_label.add_css_class("heading");
    let buffered = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    buffered.add_css_class("dim-label");
    buffered.add_css_class("caption");
    let close = gtk::Button::builder()
        .icon_name("window-close-symbolic")
        .tooltip_text("Close (Esc)")
        .valign(gtk::Align::Center)
        .build();
    close.add_css_class("flat");
    close.add_css_class("circular");
    {
        let window = window.clone();
        close.connect_clicked(move |_| window.close());
    }
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let header_text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    header_text.set_hexpand(true);
    header_text.set_valign(gtk::Align::Center);
    header_text.append(&state_label);
    header_text.append(&buffered);
    header.append(&dot);
    header.append(&header_text);
    header.append(&close);

    // Clip buttons
    let toasts = adw::ToastOverlay::new();
    let clip_row = crate::clip_buttons::build(state, &toasts, "clip-button");

    let custom_secs = gtk::SpinButton::with_range(1.0, 600.0, 5.0);
    custom_secs.set_value(15.0);
    custom_secs.set_valign(gtk::Align::Center);
    custom_secs.set_tooltip_text(Some("Seconds"));
    let custom_clip = gtk::Button::with_label("Clip");
    custom_clip.set_valign(gtk::Align::Center);
    let custom_label = gtk::Label::builder().label("Custom length").xalign(0.0).hexpand(true).build();
    let seconds_unit = gtk::Label::new(Some("s"));
    seconds_unit.add_css_class("dim-label");
    let custom_row = gtk::Box::builder().spacing(8).build();
    custom_row.append(&custom_label);
    custom_row.append(&custom_secs);
    custom_row.append(&seconds_unit);
    custom_row.append(&custom_clip);

    // Quick toggles
    let pause = gtk::ToggleButton::builder().tooltip_text("Pause recording").build();
    let desktop = gtk::ToggleButton::builder()
        .icon_name("audio-speakers-symbolic")
        .tooltip_text("Desktop audio")
        .build();
    let mic = gtk::ToggleButton::builder()
        .icon_name("audio-input-microphone-symbolic")
        .tooltip_text("Microphone")
        .build();
    let quality = gtk::DropDown::from_strings(&QUALITIES.map(|q| q.1));
    quality.set_tooltip_text(Some("Video quality. Changing it restarts the buffer"));
    quality.set_hexpand(true);
    let toggles = gtk::Box::builder().spacing(8).build();
    toggles.append(&pause);
    toggles.append(&desktop);
    toggles.append(&mic);
    toggles.append(&quality);

    // Last saved clip, with a quick rename
    let saved_title = gtk::Entry::builder()
        .placeholder_text("Name this clip")
        .hexpand(true)
        .build();
    let saved_label = gtk::Label::builder().xalign(0.0).build();
    saved_label.add_css_class("caption");
    saved_label.add_css_class("success");
    let saved_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    saved_box.add_css_class("saved-card");
    saved_box.append(&saved_label);
    saved_box.append(&saved_title);
    let saved = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .child(&saved_box)
        .build();

    // Footer
    let open_library = gtk::Button::with_label("Library");
    open_library.add_css_class("flat");
    let open_settings = gtk::Button::with_label("All settings");
    open_settings.add_css_class("flat");
    let footer = gtk::Box::builder().spacing(4).homogeneous(true).build();
    footer.append(&open_library);
    footer.append(&open_settings);

    let offline = adw::StatusPage::builder()
        .icon_name("media-record-symbolic")
        .title("Recorder isn't running")
        .build();
    offline.add_css_class("compact");
    let start = gtk::Button::with_label("Start recorder");
    start.add_css_class("suggested-action");
    start.add_css_class("pill");
    start.set_halign(gtk::Align::Center);
    start.connect_clicked(|_| util::start_daemon());
    offline.set_child(Some(&start));

    let controls = gtk::Box::new(gtk::Orientation::Vertical, 12);
    controls.append(&clip_row);
    controls.append(&custom_row);
    controls.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    controls.append(&toggles);
    controls.append(&saved);

    let body = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vhomogeneous(false)
        .build();
    body.add_named(&controls, Some("controls"));
    body.add_named(&offline, Some("offline"));

    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(14)
        .margin_start(16)
        .margin_end(16)
        .margin_top(14)
        .margin_bottom(10)
        .build();
    root.append(&header);
    root.append(&body);
    root.append(&footer);

    toasts.set_child(Some(&root));
    window.set_content(Some(&toasts));

    // --- State -> widgets -----------------------------------------------------------------
    let syncing = Rc::new(std::cell::Cell::new(false));
    let refresh = {
        let state = state.clone();
        let syncing = syncing.clone();
        let (dot, state_label, buffered, body) = (dot.clone(), state_label.clone(), buffered.clone(), body.clone());
        let (custom_secs, custom_clip) = (custom_secs.clone(), custom_clip.clone());
        let (pause, desktop, mic, quality) = (pause.clone(), desktop.clone(), mic.clone(), quality.clone());
        move || {
            let status = state.status();
            let cfg = state.config();
            util::set_dot_state(&dot, util::state_class(status.as_ref()));
            state_label.set_label(util::state_label(status.as_ref()));
            body.set_visible_child_name(if status.is_some() { "controls" } else { "offline" });
            buffered.set_label(&match &status {
                Some(s) if s.state == CaptureState::Recording => format!(
                    "{:.0} of {} s buffered",
                    s.buffered_secs.min(cfg.longest_clip_secs() as f32),
                    cfg.longest_clip_secs()
                ),
                Some(s) => util::status_line(s),
                None => String::new(),
            });
            buffered.set_visible(status.is_some());

            let recording = matches!(status.as_ref().map(|s| s.state), Some(CaptureState::Recording));
            custom_clip.set_sensitive(recording);
            let max = (cfg.longest_clip_secs()) as f64;
            custom_secs.set_range(1.0, max);

            syncing.set(true);
            let paused = state.paused();
            pause.set_active(paused);
            pause.set_icon_name(if paused { "media-playback-start-symbolic" } else { "media-playback-pause-symbolic" });
            pause.set_tooltip_text(Some(if paused { "Resume recording" } else { "Pause recording" }));
            desktop.set_active(cfg.audio.capture_desktop);
            mic.set_active(cfg.audio.capture_mic);
            let qi = QUALITIES.iter().position(|q| q.0 == cfg.video.quality);
            quality.set_sensitive(qi.is_some());
            if let Some(i) = qi {
                quality.set_selected(i as u32);
            }
            syncing.set(false);
        }
    };
    refresh();
    state.connect_changed(refresh);

    // --- Clips ----------------------------------------------------------------------------
    let last_clip: Rc<RefCell<Option<ClipMeta>>> = Rc::default();
    let show_saved = {
        let last_clip = last_clip.clone();
        let (saved, saved_label, saved_title) = (saved.clone(), saved_label.clone(), saved_title.clone());
        move |clip: &ClipMeta| {
            saved_label.set_label(&format!("Saved {} clip", util::duration(clip.duration_secs)));
            saved_title.set_text(&clip.title);
            *last_clip.borrow_mut() = Some(clip.clone());
            saved.set_reveal_child(true);
        }
    };
    state.connect_clip_saved(show_saved);

    let save = {
        let state = state.clone();
        let toasts = toasts.clone();
        Rc::new(move |secs: u32, button: gtk::Button| {
            let state = state.clone();
            let toasts = toasts.clone();
            button.set_sensitive(false);
            glib::spawn_future_local(async move {
                if let Err(e) = state.save_clip(Some(secs)).await {
                    toasts.add_toast(util::toast(&format!("Couldn't save: {e}")));
                }
                button.set_sensitive(true);
            });
        })
    };
    {
        let save = save.clone();
        let custom_secs = custom_secs.clone();
        custom_clip.connect_clicked(move |b| save(custom_secs.value() as u32, b.clone()));
    }

    // Rename the last clip on Enter.
    {
        let state = state.clone();
        let last_clip = last_clip.clone();
        let saved_label = saved_label.clone();
        let toasts = toasts.clone();
        saved_title.connect_activate(move |entry| {
            let title = entry.text().trim().to_string();
            let Some(mut clip) = last_clip.borrow().clone() else { return };
            if title.is_empty() || title == clip.title {
                return;
            }
            clip.title = title;
            let lib = state.library();
            let to_write = clip.clone();
            let last_clip = last_clip.clone();
            let saved_label = saved_label.clone();
            let toasts = toasts.clone();
            glib::spawn_future_local(async move {
                let result = gtk::gio::spawn_blocking(move || lib.update(&to_write))
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("writer panicked")));
                match result {
                    Ok(()) => {
                        saved_label.set_label("Renamed");
                        *last_clip.borrow_mut() = Some(clip);
                    }
                    Err(e) => toasts.add_toast(util::toast(&format!("Couldn't rename: {e}"))),
                }
            });
        });
    }

    // --- Toggles --------------------------------------------------------------------------
    {
        let state = state.clone();
        let syncing = syncing.clone();
        let toasts = toasts.clone();
        pause.connect_toggled(move |b| {
            if syncing.get() {
                return;
            }
            let state = state.clone();
            let toasts = toasts.clone();
            let paused = b.is_active();
            glib::spawn_future_local(async move {
                if let Err(e) = state.set_paused(paused).await {
                    toasts.add_toast(util::toast(&format!("{e}")));
                }
            });
        });
    }
    let apply = {
        let state = state.clone();
        let toasts = toasts.clone();
        Rc::new(move |f: Box<dyn FnOnce(&mut clippr_core::config::Config)>| {
            let mut cfg = state.config();
            f(&mut cfg);
            let state = state.clone();
            let toasts = toasts.clone();
            glib::spawn_future_local(async move {
                if let Err(e) = state.apply_config(cfg).await {
                    toasts.add_toast(util::toast(&format!("{e}")));
                }
            });
        })
    };
    {
        let (apply, syncing) = (apply.clone(), syncing.clone());
        desktop.connect_toggled(move |b| {
            if !syncing.get() {
                let on = b.is_active();
                apply(Box::new(move |c| c.audio.capture_desktop = on));
            }
        });
    }
    {
        let (apply, syncing) = (apply.clone(), syncing.clone());
        mic.connect_toggled(move |b| {
            if !syncing.get() {
                let on = b.is_active();
                apply(Box::new(move |c| c.audio.capture_mic = on));
            }
        });
    }
    {
        let (apply, syncing) = (apply.clone(), syncing.clone());
        quality.connect_selected_notify(move |d| {
            if !syncing.get() {
                let q = QUALITIES[d.selected() as usize % QUALITIES.len()].0;
                apply(Box::new(move |c| c.video.quality = q));
            }
        });
    }

    // --- Footer and keys ------------------------------------------------------------------
    {
        let window = window.clone();
        open_library.connect_clicked(move |_| {
            util::spawn_detached(util::sibling_exe("clippr-gui"), &[]);
            window.close();
        });
    }
    {
        let window = window.clone();
        open_settings.connect_clicked(move |_| {
            util::spawn_detached(util::sibling_exe("clippr-gui"), &["--settings"]);
            window.close();
        });
    }
    let keys = gtk::EventControllerKey::new();
    {
        let window = window.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                window.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }
    window.add_controller(keys);

    window
}
