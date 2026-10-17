//! The main window: clip library and settings tabs, with the editor pushed on top.

use std::rc::Rc;

use adw::prelude::*;
use clippr_core::clip::ClipMeta;

use crate::state::State;
use crate::{editor, library, settings};

const STACK_NAME: &str = "clippr-view-stack";

pub fn build(app: &adw::Application, state: &Rc<State>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Clippr")
        .default_width(1120)
        .default_height(740)
        .width_request(360)
        .height_request(420)
        .build();

    let toasts = adw::ToastOverlay::new();
    let nav = adw::NavigationView::new();

    let stack = adw::ViewStack::new();
    stack.set_widget_name(STACK_NAME);

    let open_editor: Rc<dyn Fn(ClipMeta)> = {
        let nav = nav.clone();
        let state = state.clone();
        let toasts = toasts.clone();
        Rc::new(move |clip| {
            let page = editor::build(&state, &toasts, clip);
            nav.push(&page);
        })
    };

    let clips = library::build(state, &toasts, open_editor.clone());
    stack.add_titled_with_icon(&clips, Some("clips"), "Clips", "camera-video-symbolic");
    let prefs = settings::build(state, &toasts);
    stack.add_titled_with_icon(&prefs, Some("settings"), "Settings", "emblem-system-symbolic");

    let switcher = adw::ViewSwitcher::builder()
        .stack(&stack)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();
    let header = adw::HeaderBar::builder().title_widget(&switcher).build();

    // Narrow windows move the tabs to a bottom bar.
    let bottom = adw::ViewSwitcherBar::builder().stack(&stack).build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_bottom_bar(&bottom);
    toolbar.set_content(Some(&stack));

    let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        560.0,
        adw::LengthUnit::Sp,
    ));
    breakpoint.add_setter(&header, "title-widget", Some(&None::<gtk::Widget>.to_value()));
    breakpoint.add_setter(&bottom, "reveal", Some(&true.to_value()));
    window.add_breakpoint(breakpoint);

    let root = adw::NavigationPage::builder()
        .title("Clippr")
        .tag("root")
        .child(&toolbar)
        .build();
    nav.add(&root);

    toasts.set_child(Some(&nav));
    window.set_content(Some(&toasts));

    #[cfg(debug_assertions)]
    if let Ok(id) = std::env::var("CLIPPR_GUI_OPEN_CLIP") {
        if let Ok(clip) = state.library().get(&id) {
            open_editor(clip);
        }
    }
    window
}

/// Switch an existing main window to the settings tab, closing any open editor.
pub fn show_settings(window: &adw::ApplicationWindow) {
    let Some(content) = window.content() else { return };
    let Some(toasts) = content.downcast_ref::<adw::ToastOverlay>() else { return };
    let Some(nav) = toasts.child().and_downcast::<adw::NavigationView>() else { return };
    nav.pop_to_tag("root");
    if let Some(stack) = find_stack(nav.upcast_ref()) {
        stack.set_visible_child_name("settings");
    }
}

fn find_stack(widget: &gtk::Widget) -> Option<adw::ViewStack> {
    if widget.widget_name() == STACK_NAME {
        return widget.clone().downcast().ok();
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(found) = find_stack(&c) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}
