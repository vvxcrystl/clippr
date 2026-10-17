//! clippr-toast: a short "Clip saved" popup in the screen corner, styled like the library's
//! clip cards (thumbnail, duration badge, title and caption) on an Adwaita popover surface.
//!
//! It draws on the Wayland layer-shell overlay layer, so it shows above fullscreen games,
//! never takes keyboard focus and lets clicks pass through. It exits on its own after a few
//! seconds. Rendering is done in software into a shared-memory buffer, so there is no GUI
//! toolkit to load.
//!
//! Usage: `clippr-toast [options] [--] <title> [detail]`
//!   --seconds N        how long to stay up (default 3)
//!   --duration TEXT    badge text drawn on the thumbnail, e.g. "0:30"
//!   --thumbnail PATH   JPEG to show; if it doesn't exist yet it is picked up when it appears
//!   --accent #RRGGBB   accent color for the check mark (the desktop's accent)
//!   --light            light color scheme
//!
//! Exit codes: 0 shown, 2 bad usage, 3 no Wayland layer shell, 4 no usable font.

use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, QueueHandle,
};

const EXIT_USAGE: i32 = 2;
const EXIT_NO_LAYER_SHELL: i32 = 3;
const EXIT_NO_FONT: i32 = 4;

// Layout in logical pixels. The surface is the card plus room for its shadow on every side.
const SHADOW: f32 = 14.0;
const CARD_H: f32 = 74.0;
const CARD_MIN_W: f32 = 270.0;
const CARD_MAX_W: f32 = 460.0;
const CARD_RADIUS: f32 = 12.0;
const PAD: f32 = 10.0;
const THUMB_W: f32 = 96.0;
const THUMB_H: f32 = 54.0;
const THUMB_RADIUS: f32 = 8.0;
const TEXT_X: f32 = PAD + THUMB_W + 12.0;
const CHECK_D: f32 = 16.0;
const TITLE_X: f32 = TEXT_X + CHECK_D + 6.0;
const PAD_RIGHT: f32 = 18.0;
const TITLE_PX: f32 = 14.5;
const CAPTION_PX: f32 = 12.5;
const BADGE_PX: f32 = 11.0;
/// Distance from the screen edges to the card (not the shadow).
const SCREEN_MARGIN: f32 = 24.0;

type Rgba = [f32; 4];

struct Theme {
    card: Rgba,
    border: Rgba,
    fg: Rgba,
    dim: Rgba,
    placeholder: Rgba,
    shadow: f32,
    accent: Rgba,
}

impl Theme {
    /// Libadwaita's popover colors for each scheme.
    fn new(light: bool, accent: Rgba) -> Self {
        if light {
            Theme {
                card: [1.0, 1.0, 1.0, 1.0],
                border: [0.0, 0.0, 0.0, 0.10],
                fg: [0.0, 0.0, 0.024, 0.85],
                dim: [0.0, 0.0, 0.024, 0.55],
                placeholder: [0.0, 0.0, 0.024, 0.08],
                shadow: 0.22,
                accent,
            }
        } else {
            Theme {
                card: [0.212, 0.212, 0.227, 1.0], // #36363a
                border: [0.0, 0.0, 0.0, 0.55],
                fg: [1.0, 1.0, 1.0, 1.0],
                dim: [1.0, 1.0, 1.0, 0.55],
                placeholder: [1.0, 1.0, 1.0, 0.08],
                shadow: 0.45,
                accent,
            }
        }
    }
}

struct Args {
    seconds: f32,
    title: String,
    detail: String,
    duration: String,
    thumbnail: Option<PathBuf>,
    accent: Rgba,
    light: bool,
}

fn parse_args() -> Option<Args> {
    let mut args = Args {
        seconds: 3.0,
        title: String::new(),
        detail: String::new(),
        duration: String::new(),
        thumbnail: None,
        // Adwaita's default blue, used when the desktop doesn't report an accent.
        accent: [0.208, 0.518, 0.894, 1.0],
        light: false,
    };
    let mut positional = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            // Everything after `--` is text, so a title like "--light" can't become an option.
            "--" => {
                positional.extend(it.by_ref());
                break;
            }
            "--seconds" => {
                args.seconds = it.next()?.parse().ok().filter(|s: &f32| (0.5..=30.0).contains(s))?
            }
            "--duration" => args.duration = clean(&it.next()?, 12),
            "--thumbnail" => args.thumbnail = Some(PathBuf::from(it.next()?)),
            "--accent" => args.accent = parse_hex(&it.next()?)?,
            "--light" => args.light = true,
            "--help" | "-h" => return None,
            _ => positional.push(arg),
        }
    }
    let mut positional = positional.into_iter();
    // Keep untrusted text (titles come from window classes) to a sane size.
    args.title = clean(&positional.next()?, 80);
    args.detail = clean(&positional.next().unwrap_or_default(), 160);
    Some(args)
}

fn parse_hex(text: &str) -> Option<Rgba> {
    let hex = text.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some([((v >> 16) & 0xff) as f32 / 255.0, ((v >> 8) & 0xff) as f32 / 255.0, (v & 0xff) as f32 / 255.0, 1.0])
}

fn clean(text: &str, max: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max).collect()
}

fn main() {
    let Some(args) = parse_args() else {
        eprintln!("usage: clippr-toast [--seconds N] [--duration TEXT] [--thumbnail PATH] [--accent #RRGGBB] [--light] <title> [detail]");
        std::process::exit(EXIT_USAGE);
    };
    let Some(fonts) = Fonts::load() else {
        eprintln!("clippr-toast: no usable font found");
        std::process::exit(EXIT_NO_FONT);
    };
    let theme = Theme::new(args.light, args.accent);
    let card_w = card_width(&fonts, &args);

    // Debug builds: CLIPPR_TOAST_DUMP=<file> writes raw BGRA pixels at 2x and exits, for
    // checking the design without a compositor (convert with `magick -size WxH -depth 8 bgra:<file>`).
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("CLIPPR_TOAST_DUMP") {
        let thumb = args.thumbnail.as_deref().and_then(Thumbnail::load);
        let (w, h) = surface_size(card_w);
        let mut data = vec![0u8; (w * 2 * h * 2 * 4) as usize];
        render(&mut data, &Scene { fonts: &fonts, args: &args, theme: &theme, thumb: thumb.as_ref(), card_w }, 2);
        std::fs::write(&path, data).expect("write dump");
        println!("{}x{}", w * 2, h * 2);
        return;
    }

    let Ok(conn) = Connection::connect_to_env() else {
        std::process::exit(EXIT_NO_LAYER_SHELL);
    };
    let Ok((globals, mut queue)) = registry_queue_init::<Toast>(&conn) else {
        std::process::exit(EXIT_NO_LAYER_SHELL);
    };
    let qh = queue.handle();
    let (Ok(compositor), Ok(layer_shell), Ok(shm)) = (
        CompositorState::bind(&globals, &qh),
        LayerShell::bind(&globals, &qh),
        Shm::bind(&globals, &qh),
    ) else {
        std::process::exit(EXIT_NO_LAYER_SHELL);
    };

    let (w, h) = surface_size(card_w);
    let surface = compositor.create_surface(&qh);
    // An empty input region makes the toast click-through.
    if let Ok(region) = Region::new(&compositor) {
        surface.set_input_region(Some(region.wl_region()));
    }
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("clippr-toast"), None);
    layer.set_anchor(Anchor::TOP | Anchor::RIGHT);
    let margin = (SCREEN_MARGIN - SHADOW) as i32;
    layer.set_margin(margin, margin, 0, 0);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_size(w, h);
    layer.commit();

    let Ok(pool) = SlotPool::new((w * h * 4 * 4) as usize, &shm) else {
        std::process::exit(EXIT_NO_LAYER_SHELL);
    };
    let thumb = args.thumbnail.as_deref().and_then(Thumbnail::load);
    let mut toast = Toast {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer,
        fonts,
        theme,
        card_w,
        thumb,
        args,
        scale: 1,
        configured: false,
        closed: false,
    };

    let deadline = Instant::now() + Duration::from_secs_f32(toast.args.seconds);
    while !toast.closed {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        // Wake up briefly while the thumbnail is still being generated, otherwise sleep until
        // the compositor talks to us or it's time to go.
        let waiting = toast.thumb.is_none() && toast.args.thumbnail.is_some();
        let timeout = if waiting { Duration::from_millis(100).min(deadline - now) } else { deadline - now };
        if pump(&mut queue, &mut toast, timeout).is_err() {
            break;
        }
        if waiting {
            if let Some(thumb) = toast.args.thumbnail.as_deref().and_then(Thumbnail::load) {
                toast.thumb = Some(thumb);
                if toast.configured {
                    toast.draw();
                }
            }
        }
    }
}

/// Dispatch Wayland events, waiting at most `timeout` for new ones.
fn pump(queue: &mut wayland_client::EventQueue<Toast>, toast: &mut Toast, timeout: Duration) -> Result<(), ()> {
    queue.dispatch_pending(toast).map_err(drop)?;
    queue.flush().map_err(drop)?;
    if let Some(guard) = queue.prepare_read() {
        let mut fd = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: one valid pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut fd, 1, ms) };
        if ready > 0 {
            guard.read().map_err(drop)?;
        }
    }
    queue.dispatch_pending(toast).map_err(drop)?;
    Ok(())
}

struct Fonts {
    bold: FontVec,
    regular: FontVec,
}

impl Fonts {
    fn load() -> Option<Self> {
        let regular = load_font("sans-serif")?;
        let bold = load_font("sans-serif:bold").or_else(|| load_font("sans-serif"))?;
        Some(Self { bold, regular })
    }
}

/// Resolve a font through fontconfig (the same one GTK uses), with common paths as fallback.
fn load_font(pattern: &str) -> Option<FontVec> {
    let from_fc = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", pattern])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .filter(|p| !p.is_empty());
    let fallbacks = if pattern.contains("bold") {
        ["/usr/share/fonts/noto/NotoSans-Bold.ttf", "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf"]
    } else {
        ["/usr/share/fonts/noto/NotoSans-Regular.ttf", "/usr/share/fonts/TTF/DejaVuSans.ttf"]
    };
    from_fc
        .into_iter()
        .chain(fallbacks.iter().map(|s| s.to_string()))
        .find_map(|path| FontVec::try_from_vec(std::fs::read(path).ok()?).ok())
}

/// Decoded RGB thumbnail.
struct Thumbnail {
    rgb: Vec<u8>,
    width: usize,
    height: usize,
}

impl Thumbnail {
    fn load(path: &std::path::Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        // Thumbnails are small; refuse anything that isn't.
        if !meta.is_file() || meta.len() > 4 * 1024 * 1024 {
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        let mut decoder = zune_jpeg::JpegDecoder::new(&bytes);
        let rgb = decoder.decode().ok()?;
        let (width, height) = decoder.dimensions()?;
        (width > 0 && height > 0 && rgb.len() >= width * height * 3).then_some(Self { rgb, width, height })
    }

    /// Bilinear sample at normalized coordinates (u, v) in 0..1.
    fn sample(&self, u: f32, v: f32) -> [f32; 3] {
        let x = (u * self.width as f32 - 0.5).clamp(0.0, (self.width - 1) as f32);
        let y = (v * self.height as f32 - 0.5).clamp(0.0, (self.height - 1) as f32);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.width - 1), (y0 + 1).min(self.height - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let px = |x: usize, y: usize, c: usize| self.rgb[(y * self.width + x) * 3 + c] as f32 / 255.0;
        let mut out = [0.0; 3];
        for (c, o) in out.iter_mut().enumerate() {
            let top = px(x0, y0, c) * (1.0 - fx) + px(x1, y0, c) * fx;
            let bottom = px(x0, y1, c) * (1.0 - fx) + px(x1, y1, c) * fx;
            *o = top * (1.0 - fy) + bottom * fy;
        }
        out
    }
}

fn text_width(font: &FontVec, px: f32, text: &str) -> f32 {
    let scaled = font.as_scaled(PxScale::from(px));
    let mut width = 0.0;
    let mut prev = None;
    for c in text.chars() {
        let id = scaled.glyph_id(c);
        if let Some(p) = prev {
            width += scaled.kern(p, id);
        }
        width += scaled.h_advance(id);
        prev = Some(id);
    }
    width
}

fn card_width(fonts: &Fonts, args: &Args) -> f32 {
    let title = TITLE_X + text_width(&fonts.bold, TITLE_PX, &args.title);
    let detail = TEXT_X + text_width(&fonts.regular, CAPTION_PX, &args.detail);
    (title.max(detail) + PAD_RIGHT).ceil().clamp(CARD_MIN_W, CARD_MAX_W)
}

fn surface_size(card_w: f32) -> (u32, u32) {
    ((card_w + 2.0 * SHADOW) as u32, (CARD_H + 2.0 * SHADOW) as u32)
}

/// Shorten `text` with an ellipsis until it fits in `max` pixels.
fn ellipsize(font: &FontVec, px: f32, text: &str, max: f32) -> String {
    if text_width(font, px, text) <= max {
        return text.to_string();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let candidate = chars.iter().collect::<String>().trim_end().to_string() + "\u{2026}";
        if text_width(font, px, &candidate) <= max {
            return candidate;
        }
    }
    String::new()
}

struct Toast {
    registry: RegistryState,
    outputs: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    fonts: Fonts,
    theme: Theme,
    card_w: f32,
    thumb: Option<Thumbnail>,
    args: Args,
    scale: i32,
    configured: bool,
    closed: bool,
}

impl Toast {
    fn draw(&mut self) {
        let s = self.scale.max(1);
        let (w, h) = surface_size(self.card_w);
        let (bw, bh) = (w as i32 * s, h as i32 * s);
        let Ok((buffer, canvas)) = self.pool.create_buffer(bw, bh, bw * 4, wl_shm::Format::Argb8888) else {
            return;
        };
        let scene = Scene {
            fonts: &self.fonts,
            args: &self.args,
            theme: &self.theme,
            thumb: self.thumb.as_ref(),
            card_w: self.card_w,
        };
        render(canvas, &scene, s);
        let surface = self.layer.wl_surface();
        surface.set_buffer_scale(s);
        surface.damage_buffer(0, 0, bw, bh);
        if buffer.attach_to(surface).is_ok() {
            self.layer.commit();
        }
    }
}

struct Scene<'a> {
    fonts: &'a Fonts,
    args: &'a Args,
    theme: &'a Theme,
    thumb: Option<&'a Thumbnail>,
    card_w: f32,
}

/// Draw the whole surface (shadow included) at `scale` into an ARGB8888 buffer.
fn render(data: &mut [u8], scene: &Scene, scale: i32) {
    let s = scale as f32;
    let (w, h) = surface_size(scene.card_w);
    let mut c = Canvas { data, width: w as usize * scale as usize, height: h as usize * scale as usize };
    c.clear();
    let t = scene.theme;
    // Card origin in device pixels.
    let (ox, oy) = (SHADOW * s, SHADOW * s);
    let (cw, ch) = (scene.card_w * s, CARD_H * s);
    let px = |v: f32| v * s;

    c.shadow(ox, oy + px(3.0), cw, ch, px(CARD_RADIUS), px(SHADOW - 2.0), t.shadow);
    c.rounded_rect(ox, oy, cw, ch, px(CARD_RADIUS), t.border);
    c.rounded_rect(ox + px(1.0), oy + px(1.0), cw - px(2.0), ch - px(2.0), px(CARD_RADIUS - 1.0), t.card);

    // Thumbnail, like a library card: rounded, cropped to fill, duration badge in the corner.
    let (tx, ty, tw, th) = (ox + px(PAD), oy + (ch - px(THUMB_H)) / 2.0, px(THUMB_W), px(THUMB_H));
    match scene.thumb {
        Some(thumb) => c.image(thumb, tx, ty, tw, th, px(THUMB_RADIUS)),
        None => c.rounded_rect(tx, ty, tw, th, px(THUMB_RADIUS), t.placeholder),
    }
    if !scene.args.duration.is_empty() {
        let bw = text_width(&scene.fonts.bold, px(BADGE_PX), &scene.args.duration) + px(10.0);
        let bh = px(BADGE_PX + 5.0);
        let (bx, by) = (tx + tw - bw - px(4.0), ty + th - bh - px(4.0));
        c.rounded_rect(bx, by, bw, bh, px(5.0), [0.0, 0.0, 0.0, 0.7]);
        c.text(&scene.fonts.bold, px(BADGE_PX), bx + px(5.0), by + bh - px(4.0), &scene.args.duration, [1.0, 1.0, 1.0, 1.0]);
    }

    // Check mark in the accent color, then the title and caption.
    let max_title = cw - px(TITLE_X + PAD_RIGHT);
    let max_detail = cw - px(TEXT_X + PAD_RIGHT);
    let title = ellipsize(&scene.fonts.bold, px(TITLE_PX), &scene.args.title, max_title);
    let detail = ellipsize(&scene.fonts.regular, px(CAPTION_PX), &scene.args.detail, max_detail);
    let title_base = if detail.is_empty() { oy + ch / 2.0 + px(5.0) } else { oy + px(32.0) };
    let (kx, ky, kr) = (ox + px(TEXT_X + CHECK_D / 2.0), title_base - px(5.0), px(CHECK_D / 2.0));
    c.circle(kx, ky, kr, t.accent);
    let k = kr / 8.0;
    c.polyline(&[(kx - 3.4 * k, ky + 0.2 * k), (kx - 1.0 * k, ky + 2.6 * k), (kx + 3.6 * k, ky - 2.4 * k)], 1.5 * k, [1.0, 1.0, 1.0, 1.0]);
    c.text(&scene.fonts.bold, px(TITLE_PX), ox + px(TITLE_X), title_base, &title, t.fg);
    if !detail.is_empty() {
        c.text(&scene.fonts.regular, px(CAPTION_PX), ox + px(TEXT_X), oy + px(53.0), &detail, t.dim);
    }
}

/// Premultiplied ARGB8888 canvas (little-endian bytes: B, G, R, A).
struct Canvas<'a> {
    data: &'a mut [u8],
    width: usize,
    height: usize,
}

impl Canvas<'_> {
    fn clear(&mut self) {
        self.data.fill(0);
    }

    /// Source-over blend of `color` at `coverage` (0..1) onto pixel (x, y).
    fn blend(&mut self, x: usize, y: usize, color: Rgba, coverage: f32) {
        if x >= self.width || y >= self.height || coverage <= 0.0 {
            return;
        }
        let a = (color[3] * coverage.min(1.0)).clamp(0.0, 1.0);
        let i = (y * self.width + x) * 4;
        let px = &mut self.data[i..i + 4];
        let src = [color[2] * a, color[1] * a, color[0] * a, a];
        for (ch, s) in px.iter_mut().zip(src) {
            let d = *ch as f32 / 255.0;
            *ch = ((s + d * (1.0 - a)) * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }

    /// Visit the pixels in a box, passing each pixel's index and center.
    fn each(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, mut f: impl FnMut(&mut Self, usize, usize, f32, f32)) {
        let (xa, ya) = (x0.floor().max(0.0) as usize, y0.floor().max(0.0) as usize);
        let (xb, yb) = ((x1.ceil().max(0.0) as usize).min(self.width), (y1.ceil().max(0.0) as usize).min(self.height));
        for y in ya..yb {
            for x in xa..xb {
                f(self, x, y, x as f32 + 0.5, y as f32 + 0.5);
            }
        }
    }

    fn rounded_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, color: Rgba) {
        self.each(x, y, x + w, y + h, |c, px, py, fx, fy| {
            let d = rounded_rect_sdf(fx, fy, x, y, w, h, r);
            c.blend(px, py, color, 0.5 - d);
        });
    }

    /// A soft drop shadow: full strength under the rect, fading out over `blur` pixels.
    #[allow(clippy::too_many_arguments)]
    fn shadow(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, blur: f32, strength: f32) {
        self.each(x - blur, y - blur, x + w + blur, y + h + blur, |c, px, py, fx, fy| {
            let d = rounded_rect_sdf(fx, fy, x, y, w, h, r);
            let t = (1.0 - d.max(0.0) / blur).clamp(0.0, 1.0);
            c.blend(px, py, [0.0, 0.0, 0.0, strength], t * t * t);
        });
    }

    fn circle(&mut self, cx: f32, cy: f32, r: f32, color: Rgba) {
        self.each(cx - r - 1.0, cy - r - 1.0, cx + r + 1.0, cy + r + 1.0, |c, px, py, fx, fy| {
            let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() - r;
            c.blend(px, py, color, 0.5 - d);
        });
    }

    /// Stroke a polyline with round joins and caps.
    fn polyline(&mut self, points: &[(f32, f32)], width: f32, color: Rgba) {
        let half = width / 2.0;
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for &(x, y) in points {
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
        self.each(x0 - width, y0 - width, x1 + width, y1 + width, |c, px, py, fx, fy| {
            let d = points
                .windows(2)
                .map(|seg| segment_distance(fx, fy, seg[0], seg[1]))
                .fold(f32::MAX, f32::min)
                - half;
            c.blend(px, py, color, 0.5 - d);
        });
    }

    /// Draw `img` cropped to fill the box (like `object-fit: cover`), with rounded corners.
    #[allow(clippy::too_many_arguments)]
    fn image(&mut self, img: &Thumbnail, x: f32, y: f32, w: f32, h: f32, r: f32) {
        let (iw, ih) = (img.width as f32, img.height as f32);
        let fit = (w / iw).max(h / ih);
        let (vw, vh) = (w / (iw * fit), h / (ih * fit)); // visible fraction of the image
        let (u0, v0) = ((1.0 - vw) / 2.0, (1.0 - vh) / 2.0);
        self.each(x, y, x + w, y + h, |c, px, py, fx, fy| {
            let cover = 0.5 - rounded_rect_sdf(fx, fy, x, y, w, h, r);
            if cover <= 0.0 {
                return;
            }
            let [red, green, blue] = img.sample(u0 + (fx - x) / w * vw, v0 + (fy - y) / h * vh);
            c.blend(px, py, [red, green, blue, 1.0], cover);
        });
    }

    fn text(&mut self, font: &FontVec, px: f32, x: f32, baseline: f32, text: &str, color: Rgba) {
        let scaled = font.as_scaled(PxScale::from(px));
        let mut caret = x;
        let mut prev = None;
        for ch in text.chars() {
            let id = scaled.glyph_id(ch);
            if let Some(p) = prev {
                caret += scaled.kern(p, id);
            }
            let glyph = id.with_scale_and_position(PxScale::from(px), ab_glyph::point(caret, baseline));
            caret += scaled.h_advance(id);
            prev = Some(id);
            if let Some(outline) = font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|gx, gy, coverage| {
                    let (x, y) = (bounds.min.x + gx as f32, bounds.min.y + gy as f32);
                    if x >= 0.0 && y >= 0.0 {
                        self.blend(x as usize, y as usize, color, coverage);
                    }
                });
            }
        }
    }
}

fn rounded_rect_sdf(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let (cx, cy, hw, hh) = (x + w / 2.0, y + h / 2.0, w / 2.0, h / 2.0);
    let qx = (px - cx).abs() - (hw - r);
    let qy = (py - cy).abs() - (hh - r);
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
}

fn segment_distance(px: f32, py: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 { (((px - a.0) * dx + (py - a.1) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

impl CompositorHandler for Toast {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, factor: i32) {
        if factor != self.scale {
            self.scale = factor;
            if self.configured {
                self.draw();
            }
        }
    }
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for Toast {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Toast {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.closed = true;
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface, _: LayerSurfaceConfigure, _: u32) {
        self.configured = true;
        self.draw();
    }
}

impl ShmHandler for Toast {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Toast {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

delegate_registry!(Toast);
smithay_client_toolkit::delegate_dispatch2!(Toast);
