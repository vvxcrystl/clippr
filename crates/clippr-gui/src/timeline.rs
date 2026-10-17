//! Editor timeline: the kept segments of a clip, a playhead, and drag handles for trimming.
//!
//! The model is a sorted list of non-overlapping ranges to keep. Everything between them
//! is cut. Splitting and deleting parts edits that list; dragging an edge trims a part.

use std::cell::RefCell;
use std::rc::Rc;

use clippr_core::edit::Segment;
use gtk::prelude::*;
use gtk::{cairo, gdk};

use crate::util;

/// Shortest part the editor allows, in seconds.
const MIN_PART: f64 = 0.2;
/// Horizontal inset of the track inside the widget.
const PAD: f64 = 14.0;
/// Height of the time ruler above the track.
const RULER: f64 = 20.0;
/// How close to a part edge a press must be to grab it, in pixels.
const GRAB: f64 = 9.0;

#[derive(Clone, Copy)]
enum Drag {
    Scrub,
    Edge { part: usize, start: bool },
}

struct Model {
    duration: f64,
    parts: Vec<Segment>,
    selected: Option<usize>,
    playhead: f64,
    history: Vec<Vec<Segment>>,
    drag: Option<Drag>,
    drag_origin: f64,
}

type Slot<F> = Rc<RefCell<Option<Box<F>>>>;

#[derive(Clone)]
pub struct Timeline {
    area: gtk::DrawingArea,
    model: Rc<RefCell<Model>>,
    on_seek: Slot<dyn Fn(f64)>,
    on_changed: Slot<dyn Fn()>,
}

impl Timeline {
    pub fn new(duration: f64) -> Self {
        let duration = duration.max(MIN_PART);
        let area = gtk::DrawingArea::builder()
            .content_height(76)
            .hexpand(true)
            .focusable(true)
            .build();
        area.add_css_class("timeline");
        area.set_cursor_from_name(Some("pointer"));

        let timeline = Self {
            area,
            model: Rc::new(RefCell::new(Model {
                duration,
                parts: vec![Segment { start_secs: 0.0, end_secs: duration }],
                selected: Some(0),
                playhead: 0.0,
                history: Vec::new(),
                drag: None,
                drag_origin: 0.0,
            })),
            on_seek: Rc::default(),
            on_changed: Rc::default(),
        };

        let model = timeline.model.clone();
        timeline.area.set_draw_func(move |area, cr, w, h| {
            draw(area, cr, w as f64, h as f64, &model.borrow());
        });

        timeline.install_gestures();
        timeline
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    pub fn connect_seek(&self, f: impl Fn(f64) + 'static) {
        *self.on_seek.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        *self.on_changed.borrow_mut() = Some(Box::new(f));
    }

    pub fn set_playhead(&self, t: f64) {
        let mut m = self.model.borrow_mut();
        // Ignore playback updates while the user drags the playhead.
        if matches!(m.drag, Some(Drag::Scrub)) {
            return;
        }
        m.playhead = t.clamp(0.0, m.duration);
        drop(m);
        self.area.queue_draw();
    }

    pub fn parts(&self) -> Vec<Segment> {
        self.model.borrow().parts.clone()
    }

    pub fn duration(&self) -> f64 {
        self.model.borrow().duration
    }

    pub fn kept_secs(&self) -> f64 {
        self.model.borrow().parts.iter().map(|p| p.end_secs - p.start_secs).sum()
    }

    pub fn is_untouched(&self) -> bool {
        let m = self.model.borrow();
        m.parts.len() == 1 && m.parts[0].start_secs <= 0.001 && m.parts[0].end_secs >= m.duration - 0.001
    }

    pub fn can_undo(&self) -> bool {
        !self.model.borrow().history.is_empty()
    }

    /// Where playback should continue from when it reaches `t`, skipping cut ranges.
    /// `None` means `t` is past the last kept part.
    pub fn resolve_playback(&self, t: f64) -> Option<f64> {
        let m = self.model.borrow();
        for part in &m.parts {
            if t < part.start_secs - 0.02 {
                return Some(part.start_secs);
            }
            if t <= part.end_secs {
                return Some(t);
            }
        }
        None
    }

    pub fn first_start(&self) -> f64 {
        self.model.borrow().parts.first().map_or(0.0, |p| p.start_secs)
    }

    /// Split the part under the playhead in two.
    pub fn split(&self) -> bool {
        self.edit(|m| {
            let t = m.playhead;
            let Some(i) = m
                .parts
                .iter()
                .position(|p| t > p.start_secs + MIN_PART && t < p.end_secs - MIN_PART)
            else {
                return false;
            };
            let p = m.parts[i];
            m.parts[i].end_secs = t;
            m.parts.insert(i + 1, Segment { start_secs: t, end_secs: p.end_secs });
            m.selected = Some(i + 1);
            true
        })
    }

    /// Cut the selected part out. The last remaining part can't be removed.
    pub fn delete_selected(&self) -> bool {
        self.edit(|m| {
            let Some(i) = m.selected else { return false };
            if m.parts.len() <= 1 || i >= m.parts.len() {
                return false;
            }
            m.parts.remove(i);
            m.selected = Some(i.min(m.parts.len() - 1));
            true
        })
    }

    /// Move the start of the part under the playhead to the playhead.
    pub fn trim_start(&self) -> bool {
        self.edit(|m| {
            let t = m.playhead;
            let Some(i) = m.parts.iter().position(|p| t >= p.start_secs && t < p.end_secs - MIN_PART)
            else {
                return false;
            };
            m.parts[i].start_secs = t;
            m.selected = Some(i);
            true
        })
    }

    /// Move the end of the part under the playhead to the playhead.
    pub fn trim_end(&self) -> bool {
        self.edit(|m| {
            let t = m.playhead;
            let Some(i) = m.parts.iter().position(|p| t > p.start_secs + MIN_PART && t <= p.end_secs)
            else {
                return false;
            };
            m.parts[i].end_secs = t;
            m.selected = Some(i);
            true
        })
    }

    pub fn reset(&self) -> bool {
        self.edit(|m| {
            m.parts = vec![Segment { start_secs: 0.0, end_secs: m.duration }];
            m.selected = Some(0);
            true
        })
    }

    pub fn undo(&self) -> bool {
        let mut m = self.model.borrow_mut();
        let Some(prev) = m.history.pop() else { return false };
        m.parts = prev;
        m.selected = m.selected.map(|i| i.min(m.parts.len() - 1));
        drop(m);
        self.changed();
        true
    }

    /// Apply an edit, recording undo history only if it changed something.
    fn edit(&self, f: impl FnOnce(&mut Model) -> bool) -> bool {
        let mut m = self.model.borrow_mut();
        let before = m.parts.clone();
        if !f(&mut m) || m.parts == before {
            return false;
        }
        m.history.push(before);
        drop(m);
        self.changed();
        true
    }

    fn changed(&self) {
        self.area.queue_draw();
        if let Some(f) = self.on_changed.borrow().as_ref() {
            f();
        }
    }

    fn seek(&self, t: f64) {
        if let Some(f) = self.on_seek.borrow().as_ref() {
            f(t);
        }
    }

    fn install_gestures(&self) {
        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);

        let this = self.clone();
        drag.connect_drag_begin(move |_, x, _| {
            this.area.grab_focus();
            let width = this.area.width() as f64;
            let mut m = this.model.borrow_mut();
            let grabbed = hit_edge(&m, width, x);
            let t = time_at(&m, width, x);
            match grabbed {
                Some((part, start)) => {
                    m.drag = Some(Drag::Edge { part, start });
                    m.drag_origin = if start { m.parts[part].start_secs } else { m.parts[part].end_secs };
                    m.selected = Some(part);
                    let snapshot = m.parts.clone();
                    m.history.push(snapshot);
                }
                None => {
                    m.drag = Some(Drag::Scrub);
                    m.playhead = t;
                    if let Some(i) = m.parts.iter().position(|p| t >= p.start_secs && t <= p.end_secs) {
                        m.selected = Some(i);
                    }
                }
            }
            drop(m);
            this.area.queue_draw();
            if grabbed.is_none() {
                this.seek(t);
            }
        });

        let this = self.clone();
        drag.connect_drag_update(move |gesture, dx, _| {
            let Some((x0, _)) = gesture.start_point() else { return };
            let width = this.area.width() as f64;
            let mut m = this.model.borrow_mut();
            let t = time_at(&m, width, x0 + dx);
            match m.drag {
                Some(Drag::Scrub) => {
                    m.playhead = t;
                    drop(m);
                    this.area.queue_draw();
                    this.seek(t);
                }
                Some(Drag::Edge { part, start }) => {
                    let lo = if part == 0 { 0.0 } else { m.parts[part - 1].end_secs };
                    let hi = m.parts.get(part + 1).map_or(m.duration, |p| p.start_secs);
                    let p = m.parts[part];
                    let t = if start {
                        t.clamp(lo, p.end_secs - MIN_PART)
                    } else {
                        t.clamp(p.start_secs + MIN_PART, hi)
                    };
                    if start {
                        m.parts[part].start_secs = t;
                    } else {
                        m.parts[part].end_secs = t;
                    }
                    m.playhead = t;
                    drop(m);
                    this.area.queue_draw();
                    this.seek(t);
                }
                None => {}
            }
        });

        let this = self.clone();
        drag.connect_drag_end(move |_, _, _| {
            let mut m = this.model.borrow_mut();
            let drag = m.drag.take();
            if let Some(Drag::Edge { part, start }) = drag {
                let now = if start { m.parts[part].start_secs } else { m.parts[part].end_secs };
                if (now - m.drag_origin).abs() < 1e-6 {
                    // A click on an edge without moving it shouldn't create an undo step.
                    m.history.pop();
                    return;
                }
                drop(m);
                this.changed();
            }
        });
        self.area.add_controller(drag);

        // Hover feedback: resize cursor near edges.
        let motion = gtk::EventControllerMotion::new();
        let this = self.clone();
        motion.connect_motion(move |_, x, _| {
            let m = this.model.borrow();
            if m.drag.is_some() {
                return;
            }
            let near = hit_edge(&m, this.area.width() as f64, x).is_some();
            this.area.set_cursor_from_name(Some(if near { "ew-resize" } else { "pointer" }));
        });
        self.area.add_controller(motion);
    }
}

fn track_width(width: f64) -> f64 {
    (width - 2.0 * PAD).max(1.0)
}

fn x_at(m: &Model, width: f64, t: f64) -> f64 {
    PAD + t / m.duration * track_width(width)
}

fn time_at(m: &Model, width: f64, x: f64) -> f64 {
    ((x - PAD) / track_width(width) * m.duration).clamp(0.0, m.duration)
}

/// The part edge under `x`, preferring the selected part when two edges touch.
fn hit_edge(m: &Model, width: f64, x: f64) -> Option<(usize, bool)> {
    let mut best: Option<(usize, bool, f64)> = None;
    for (i, p) in m.parts.iter().enumerate() {
        for (start, t) in [(true, p.start_secs), (false, p.end_secs)] {
            let d = (x_at(m, width, t) - x).abs();
            let bias = if m.selected == Some(i) { 0.5 } else { 0.0 };
            if d <= GRAB && best.is_none_or(|(_, _, bd)| d - bias < bd) {
                best = Some((i, start, d - bias));
            }
        }
    }
    best.map(|(i, s, _)| (i, s))
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
    cr.arc(x + r, y + r, r, std::f64::consts::PI, 3.0 * std::f64::consts::FRAC_PI_2);
    cr.close_path();
}

fn set_rgba(cr: &cairo::Context, c: &gdk::RGBA, alpha: f32) {
    cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, (c.alpha() * alpha) as f64);
}

fn draw(area: &gtk::DrawingArea, cr: &cairo::Context, w: f64, h: f64, m: &Model) {
    let fg = area.color();
    let accent = adw::StyleManager::default().accent_color_rgba();
    let track_top = RULER + 4.0;
    let track_h = (h - track_top - 6.0).max(10.0);

    // Ruler: pick a tick step that keeps labels at least ~70px apart.
    let px_per_sec = track_width(w) / m.duration;
    let step = [0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0]
        .into_iter()
        .find(|s| s * px_per_sec >= 70.0)
        .unwrap_or(1200.0);
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(10.0);
    let mut t = 0.0;
    while t <= m.duration + 1e-6 {
        let x = x_at(m, w, t).round() + 0.5;
        set_rgba(cr, &fg, 0.25);
        cr.set_line_width(1.0);
        cr.move_to(x, RULER - 5.0);
        cr.line_to(x, RULER);
        let _ = cr.stroke();
        let label = if step < 1.0 { util::duration_precise(t) } else { util::duration(t) };
        if let Ok(ext) = cr.text_extents(&label) {
            let lx = (x - ext.width() / 2.0).clamp(0.0, w - ext.width());
            set_rgba(cr, &fg, 0.55);
            cr.move_to(lx, RULER - 8.0);
            let _ = cr.show_text(&label);
        }
        // Minor ticks
        for k in 1..5 {
            let mt = t + step * k as f64 / 5.0;
            if mt > m.duration {
                break;
            }
            let mx = x_at(m, w, mt).round() + 0.5;
            set_rgba(cr, &fg, 0.12);
            cr.move_to(mx, RULER - 2.0);
            cr.line_to(mx, RULER);
            let _ = cr.stroke();
        }
        t += step;
    }

    // Track background with hatching: anything not covered by a part is cut.
    let x0 = PAD;
    let tw = track_width(w);
    rounded_rect(cr, x0, track_top, tw, track_h, 8.0);
    set_rgba(cr, &fg, 0.06);
    let _ = cr.fill_preserve();
    cr.save().ok();
    cr.clip();
    set_rgba(cr, &fg, 0.10);
    cr.set_line_width(1.0);
    let mut hx = x0 - track_h;
    while hx < x0 + tw {
        cr.move_to(hx, track_top + track_h);
        cr.line_to(hx + track_h, track_top);
        hx += 7.0;
    }
    let _ = cr.stroke();
    cr.restore().ok();

    // Kept parts
    for (i, p) in m.parts.iter().enumerate() {
        let px = x_at(m, w, p.start_secs);
        let pw = (x_at(m, w, p.end_secs) - px).max(2.0);
        let selected = m.selected == Some(i);
        rounded_rect(cr, px, track_top, pw, track_h, 8.0);
        set_rgba(cr, &accent, if selected { 0.42 } else { 0.26 });
        let _ = cr.fill_preserve();
        set_rgba(cr, &accent, if selected { 1.0 } else { 0.6 });
        cr.set_line_width(if selected { 2.0 } else { 1.0 });
        let _ = cr.stroke();

        // Grab handles
        for hx in [px + 1.0, px + pw - 5.0] {
            rounded_rect(cr, hx, track_top + track_h * 0.25, 4.0, track_h * 0.5, 2.0);
            set_rgba(cr, &accent, if selected { 1.0 } else { 0.7 });
            let _ = cr.fill();
        }

        // Part length, if there is room for it.
        let label = util::duration_precise(p.end_secs - p.start_secs);
        if let Ok(ext) = cr.text_extents(&label) {
            if ext.width() + 20.0 < pw {
                set_rgba(cr, &fg, 0.8);
                cr.move_to(px + (pw - ext.width()) / 2.0, track_top + track_h / 2.0 + ext.height() / 2.0);
                let _ = cr.show_text(&label);
            }
        }
    }

    // Playhead
    let x = x_at(m, w, m.playhead).round();
    set_rgba(cr, &fg, 0.95);
    cr.rectangle(x - 1.0, RULER - 2.0, 2.0, h - RULER);
    let _ = cr.fill();
    cr.move_to(x - 5.0, RULER - 6.0);
    cr.line_to(x + 5.0, RULER - 6.0);
    cr.line_to(x, RULER);
    cr.close_path();
    let _ = cr.fill();
}
