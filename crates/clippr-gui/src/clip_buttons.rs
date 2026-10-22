//! Clip actions shared by the library and quick overlay.

use std::{cell::RefCell, rc::Rc};

use adw::prelude::*;
use clippr_core::{config::ClipBinding, ipc::CaptureState};
use gtk::glib;

use crate::{state::State, util};

pub fn build(state: &Rc<State>, toasts: &adw::ToastOverlay, style: &str) -> gtk::FlowBox {
    let buttons = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .column_spacing(8)
        .row_spacing(8)
        .homogeneous(true)
        .max_children_per_line(3)
        .build();
    let refresh = {
        let buttons = buttons.clone();
        let state = state.clone();
        let toasts = toasts.clone();
        let style = style.to_owned();
        let previous = RefCell::new(None);
        move || {
            let cfg = state.config();
            let recording = state
                .status()
                .is_some_and(|s| s.state == CaptureState::Recording);
            if previous.borrow().as_ref() != Some(&cfg.hotkeys.clips) {
                while let Some(child) = buttons.first_child() {
                    buttons.remove(&child);
                }
                let bindings = if cfg.hotkeys.clips.is_empty() {
                    vec![ClipBinding {
                        trigger: String::new(),
                        seconds: cfg.default_clip_secs(),
                    }]
                } else {
                    cfg.hotkeys.clips.clone()
                };
                buttons.set_max_children_per_line(bindings.len().clamp(1, 3) as u32);
                for (i, binding) in bindings.into_iter().enumerate() {
                    let button = gtk::Button::with_label(&format!(
                        "Clip {}",
                        util::clip_length(binding.seconds)
                    ));
                    button.add_css_class(&style);
                    if i == 0 {
                        button.add_css_class("suggested-action");
                    }
                    if !binding.trigger.is_empty() {
                        button.set_tooltip_text(Some(&binding.trigger));
                    }
                    {
                        let state = state.clone();
                        let toasts = toasts.clone();
                        button.connect_clicked(move |button| {
                            let state = state.clone();
                            let toasts = toasts.clone();
                            let button = button.clone();
                            button.set_sensitive(false);
                            glib::spawn_future_local(async move {
                                let message = match state.save_clip(Some(binding.seconds)).await {
                                    Ok(_) => "Clip saved".to_owned(),
                                    Err(e) => format!("Couldn't save clip: {e}"),
                                };
                                toasts.add_toast(util::toast(&message));
                                button.set_sensitive(
                                    state
                                        .status()
                                        .is_some_and(|s| s.state == CaptureState::Recording),
                                );
                            });
                        });
                    }
                    buttons.insert(&button, -1);
                }
                *previous.borrow_mut() = Some(cfg.hotkeys.clips);
            }
            buttons.set_sensitive(recording);
        }
    };
    refresh();
    state.connect_changed(refresh);
    buttons
}
