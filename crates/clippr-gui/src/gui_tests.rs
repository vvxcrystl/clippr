//! Isolated GTK regression checks, launched by scripts/check-editor.py.

use adw::prelude::*;
use clippr_core::{clip::AudioTrackKind, edit::TrackGain};
use gtk::glib;

use crate::{editor, state::State};

fn matching<T: IsA<gtk::Widget> + glib::object::IsClass>(
    widget: &gtk::Widget,
    predicate: &impl Fn(&T) -> bool,
) -> Option<T> {
    if let Ok(found) = widget.clone().downcast::<T>() {
        if predicate(&found) {
            return Some(found);
        }
    }
    let mut child = widget.first_child();
    while let Some(widget) = child {
        if let Some(found) = matching(&widget, predicate) {
            return Some(found);
        }
        child = widget.next_sibling();
    }
    None
}

fn find<T: IsA<gtk::Widget> + glib::object::IsClass>(widget: &gtk::Widget) -> Option<T> {
    matching(widget, &|_| true)
}

fn wait_for(condition: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !condition() {
        assert!(
            std::time::Instant::now() < deadline,
            "GTK condition timed out"
        );
        while glib::MainContext::default().pending() {
            glib::MainContext::default().iteration(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Run with scripts/check-editor.py so display, config and clips are isolated.
#[test]
#[ignore = "requires isolated GTK display and generated media fixture"]
fn editor_audio_and_shortcut_settings() {
    assert_eq!(
        std::env::var("CLIPPR_ISOLATED_GUI_TEST").as_deref(),
        Ok("1")
    );
    adw::init().unwrap();
    let state = State::new();
    let mut clip = state.library().list().unwrap().remove(0);
    clip.mix_gains = vec![TrackGain {
        kind: AudioTrackKind::Desktop,
        gain: 0.25,
    }];
    state.library().update(&clip).unwrap();
    assert_eq!(state.config().audio.levels.game, 90);
    let toasts = adw::ToastOverlay::new();
    let page = editor::build(&state, &toasts, state.library().get(&clip.id).unwrap());
    let nav = adw::NavigationView::new();
    nav.add(&adw::NavigationPage::new(
        &gtk::Box::new(gtk::Orientation::Vertical, 0),
        "Library",
    ));
    toasts.set_child(Some(&nav));
    let window = gtk::Window::builder()
        .default_width(1120)
        .default_height(740)
        .child(&toasts)
        .build();
    window.present();
    nav.push(&page);
    wait_for(|| page.is_mapped());
    let picture = find::<gtk::Picture>(page.upcast_ref()).unwrap();
    let media = picture
        .paintable()
        .unwrap()
        .downcast::<gtk::MediaFile>()
        .unwrap();
    let scale = find::<gtk::Scale>(page.upcast_ref()).unwrap();
    assert_eq!(
        scale.value(),
        25.0,
        "saved clip gain must override global 90%"
    );
    media.set_muted(true);
    wait_for(|| media.is_prepared() || media.error().is_some());
    assert_eq!(media.error(), None);
    media.play();
    wait_for(|| media.timestamp() > 100_000);
    assert!(media.is_playing());
    assert!(nav.pop());
    assert!(
        !media.is_playing(),
        "navigation must stop audio before the page is unmapped"
    );
    wait_for(|| media.file().is_none());
    assert!(picture.paintable().is_none());
    window.destroy();

    // Exercise the actual shortcut dialog and its persistence path offline.
    let settings = crate::settings::build(&state, &toasts);
    let window = adw::Window::builder()
        .default_width(800)
        .default_height(900)
        .content(&settings)
        .build();
    window.present();
    let add = matching::<gtk::Button>(&settings, &|b| {
        b.label().as_deref() == Some("Add clip shortcut")
    })
    .unwrap();
    add.emit_clicked();
    wait_for(|| window.visible_dialog().is_some());
    let dialog = window
        .visible_dialog()
        .unwrap()
        .downcast::<adw::AlertDialog>()
        .unwrap();
    let entry = find::<adw::EntryRow>(dialog.upcast_ref()).unwrap();
    let seconds = find::<adw::SpinRow>(dialog.upcast_ref()).unwrap();
    entry.set_text("F8");
    assert!(
        !dialog.is_response_enabled("save"),
        "duplicate keys must be rejected"
    );
    entry.set_text("F9");
    seconds.set_value(45.0);
    assert!(dialog.is_response_enabled("save"));
    matching::<gtk::Button>(dialog.upcast_ref(), &|b| {
        b.label().as_deref() == Some("Save")
    })
    .unwrap()
    .emit_clicked();
    wait_for(|| state.config().hotkeys.clips.len() == 2);
    assert_eq!(state.config().hotkeys.clips[1].seconds, 45);
    assert_eq!(clippr_core::config::Config::load().unwrap(), state.config());
    wait_for(|| window.visible_dialog().is_none());
    let row = matching::<adw::ActionRow>(&settings, &|r| r.title() == "F9").unwrap();
    adw::prelude::ActionRowExt::activate(&row);
    wait_for(|| window.visible_dialog().is_some());
    let dialog = window
        .visible_dialog()
        .unwrap()
        .downcast::<adw::AlertDialog>()
        .unwrap();
    find::<adw::EntryRow>(dialog.upcast_ref())
        .unwrap()
        .set_text("F7");
    find::<adw::SpinRow>(dialog.upcast_ref())
        .unwrap()
        .set_value(300.0);
    matching::<gtk::Button>(dialog.upcast_ref(), &|b| {
        b.label().as_deref() == Some("Save")
    })
    .unwrap()
    .emit_clicked();
    wait_for(|| state.config().hotkeys.clips[1].seconds == 300);
    assert_eq!(state.config().buffer_secs(), 303);
    let row = matching::<adw::ActionRow>(&settings, &|r| r.title() == "F8").unwrap();
    matching::<gtk::Button>(row.upcast_ref(), &|b| {
        b.tooltip_text().as_deref() == Some("Remove shortcut")
    })
    .unwrap()
    .emit_clicked();
    wait_for(|| state.config().hotkeys.clips.len() == 1);
    assert_eq!(state.config().hotkeys.clips[0].trigger, "F7");
    assert_eq!(clippr_core::config::Config::load().unwrap(), state.config());
    window.destroy();
}
