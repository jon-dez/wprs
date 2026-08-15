//! Visual demo of the surface-damage-to-buffer conversion in wprsd.
//!
//! Draws an animation over the whole buffer every frame and reports it with
//! `wl_surface.damage`, i.e. in surface coordinates, the way chromium does.
//! If wprsd converts that damage wrongly, the remote compositor repaints only
//! part of the window and the rest visibly freezes.
//!
//! Modes:
//!   dst    viewport destination scaling only, like chromium. Broken before
//!          the viewport fix, correct after it.
//!   src180 viewport source rectangle plus a 180 degree buffer transform.
//!          Broken before the transform fix, correct after it.
//!   rotate viewport source rectangle, cycling through all eight transforms.
//!          Before the transform fix only Normal and Flipped270 animate, since
//!          those are the two that never mirror an axis.
//!
//!   tN     pins transform N (0..7), surface-coordinate damage.
//!   bN     same as tN but reports buffer-coordinate damage for the whole
//!          buffer, which bypasses wprsd's conversion entirely.
//!
//!     WAYLAND_DISPLAY=wprs-0 viewport_damage_demo [dst|src180|rotate|tN|bN]

use std::env;
use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;

use smithay_client_toolkit::reexports::client::Connection;
use smithay_client_toolkit::reexports::client::Dispatch;
use smithay_client_toolkit::reexports::client::Proxy;
use smithay_client_toolkit::reexports::client::QueueHandle;
use smithay_client_toolkit::reexports::client::globals::GlobalListContents;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_buffer::WlBuffer;
use smithay_client_toolkit::reexports::client::protocol::wl_callback;
use smithay_client_toolkit::reexports::client::protocol::wl_callback::WlCallback;
use smithay_client_toolkit::reexports::client::protocol::wl_compositor::WlCompositor;
use smithay_client_toolkit::reexports::client::protocol::wl_output::Transform;
use smithay_client_toolkit::reexports::client::protocol::wl_registry;
use smithay_client_toolkit::reexports::client::protocol::wl_registry::WlRegistry;
use smithay_client_toolkit::reexports::client::protocol::wl_shm::Format;
use smithay_client_toolkit::reexports::client::protocol::wl_shm::WlShm;
use smithay_client_toolkit::reexports::client::protocol::wl_shm_pool::WlShmPool;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewport::WpViewport;
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_surface;
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_surface::XdgSurface;
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_toplevel;
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_wm_base;
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_wm_base::XdgWmBase;

const BUFFER_W: i32 = 1200;
const BUFFER_H: i32 = 800;

/// Surface size. Half the buffer, so the destination scale is 2x and a
/// conversion that ignores the viewport lands in the top-left quadrant.
const SURFACE_W: i32 = 600;
const SURFACE_H: i32 = 400;

struct Mode {
    transform: Transform,
    src: Option<(f64, f64, f64, f64)>,
    /// Cycle through every transform, this many frames apart.
    rotate_every: Option<u32>,
    /// Report damage in buffer coordinates instead of surface coordinates,
    /// which bypasses wprsd's conversion entirely.
    buffer_damage: bool,
}

/// `area` only participates for transforms that mirror an axis, so a wrong
/// `area` leaves Normal and Flipped270 looking correct and breaks the rest.
const TRANSFORMS: [Transform; 8] = [
    Transform::Normal,
    Transform::_90,
    Transform::_180,
    Transform::_270,
    Transform::Flipped,
    Transform::Flipped90,
    Transform::Flipped180,
    Transform::Flipped270,
];

struct State {
    configured: bool,
    frame_done: bool,
    closed: bool,
}

macro_rules! ignore_events {
    ($($iface:ty),* $(,)?) => {$(
        impl Dispatch<$iface, ()> for State {
            fn event(
                _: &mut Self,
                _: &$iface,
                _: <$iface as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}

ignore_events!(
    WlCompositor,
    WlShm,
    WlShmPool,
    WlBuffer,
    WlSurface,
    WpViewporter,
    WpViewport,
);

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        surface: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            state.configured = true;
        }
    }
}

impl Dispatch<XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            state.closed = true;
        }
    }
}

impl Dispatch<WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.frame_done = true;
        }
    }
}

/// 3x5 glyphs for the transform indicator.
fn glyph(ch: char) -> [u8; 5] {
    match ch {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '7' => [0b111, 0b001, 0b001, 0b001, 0b001],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        'F' => [0b111, 0b100, 0b111, 0b100, 0b100],
        '>' => [0b100, 0b010, 0b001, 0b010, 0b100],
        _ => [0, 0, 0, 0, 0],
    }
}

/// Rotation in degrees, with F marking the flipped variants.
fn transform_label(transform: Transform) -> &'static str {
    match transform {
        Transform::Normal => "0",
        Transform::_90 => "90",
        Transform::_180 => "180",
        Transform::_270 => "270",
        Transform::Flipped => "F0",
        Transform::Flipped90 => "F90",
        Transform::Flipped180 => "F180",
        Transform::Flipped270 => "F270",
        _ => "?",
    }
}

/// Swaps the transforms that are not their own inverse. Mirrors what
/// smithay's Transform::invert does, which the client crate does not expose.
fn invert(transform: Transform) -> Transform {
    match transform {
        Transform::_90 => Transform::_270,
        Transform::_270 => Transform::_90,
        Transform::Flipped90 => Transform::Flipped270,
        Transform::Flipped270 => Transform::Flipped90,
        other => other,
    }
}

/// Surface-local size of the buffer, which a rotating transform transposes.
fn surface_local_size(transform: Transform) -> (f64, f64) {
    match transform {
        Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270 => {
            (f64::from(BUFFER_H), f64::from(BUFFER_W))
        },
        _ => (f64::from(BUFFER_W), f64::from(BUFFER_H)),
    }
}

/// Maps a point from surface coordinates to buffer coordinates using the same
/// convention wprsd applies to damage. The label is deliberately not
/// orientation-compensated, so it rotates and mirrors along with the content.
/// That makes it an instrument: if the label is visible at all, this convention
/// agrees with the region the compositor samples. If it vanishes, the
/// convention is wrong and damage is landing where nothing is displayed.
fn surface_to_buffer(
    sx: f64,
    sy: f64,
    transform: Transform,
    src: Option<(f64, f64, f64, f64)>,
) -> (i32, i32) {
    let (area_w, area_h) = surface_local_size(transform);
    let (off_x, off_y, span_w, span_h) = src.unwrap_or((0.0, 0.0, area_w, area_h));

    let u = off_x + sx * span_w / f64::from(SURFACE_W);
    let v = off_y + sy * span_h / f64::from(SURFACE_H);

    // Same convention wprsd uses for damage: buffer_transform is what the
    // client already applied, so surface to buffer has to undo it. Getting this
    // out of step with the damage is what put the label outside the region the
    // viewport samples.
    let (bx, by) = match invert(transform) {
        Transform::Normal => (u, v),
        Transform::_90 => (area_h - v, u),
        Transform::_180 => (area_w - u, area_h - v),
        Transform::_270 => (v, area_w - u),
        Transform::Flipped => (area_w - u, v),
        Transform::Flipped90 => (area_h - v, area_w - u),
        Transform::Flipped180 => (u, area_h - v),
        Transform::Flipped270 => (v, u),
        // wl_output::Transform is non_exhaustive.
        _ => (u, v),
    };
    (bx as i32, by as i32)
}

/// Draws "<last> > <now>" in the window's top-left corner.
fn draw_indicator(
    pixels: &mut [u8],
    transform: Transform,
    src: Option<(f64, f64, f64, f64)>,
    last: Transform,
    now: Transform,
) {
    // Only show a transition when there was one.
    let text = if last == now {
        transform_label(now).to_string()
    } else {
        format!("{}>{}", transform_label(last), transform_label(now))
    };
    let chars: Vec<char> = text.chars().collect();
    let arrow = chars.iter().position(|ch| *ch == '>');

    // Shrink so the plate always fits: "F180>F270" is three times the width of
    // "90", and under a transposing transform that extent lands on the short
    // axis of the sampled region.
    const MAX_LABEL_W: f64 = 180.0;
    let scale = 7.0_f64.min((MAX_LABEL_W - 8.0) / (chars.len() as f64 * 4.0));

    let mut plot = |sx: f64, sy: f64, rgb: (u8, u8, u8)| {
        let (bx, by) = surface_to_buffer(sx, sy, transform, src);
        // 2x2 so the mapping can shrink without leaving gaps.
        for dy in 0..2 {
            for dx in 0..2 {
                let (px, py) = (bx + dx, by + dy);
                if px < 0 || py < 0 || px >= BUFFER_W || py >= BUFFER_H {
                    continue;
                }
                let i = ((py as usize) * BUFFER_W as usize + px as usize) * 4;
                pixels[i] = rgb.2;
                pixels[i + 1] = rgb.1;
                pixels[i + 2] = rgb.0;
                pixels[i + 3] = 255;
            }
        }
    };

    // A dark plate so the glyphs stay legible over the animation.
    let plate_w = 8.0 + chars.len() as f64 * 4.0 * scale;
    let plate_h = 8.0 + 5.0 * scale;
    for sy in 0..plate_h as i32 {
        for sx in 0..plate_w as i32 {
            plot(4.0 + f64::from(sx), 4.0 + f64::from(sy), (10, 10, 15));
        }
    }

    for (slot, ch) in chars.iter().enumerate() {
        // Dim what we came from, bright what we are now.
        let colour = match arrow {
            Some(at) if slot < at => (90, 90, 110),
            Some(at) if slot == at => (150, 150, 150),
            _ => (0, 255, 140),
        };
        for (row, bits) in glyph(*ch).iter().enumerate() {
            for col in 0..3 {
                if bits & (1 << (2 - col)) == 0 {
                    continue;
                }
                let ox = 8.0 + slot as f64 * 4.0 * scale + f64::from(col) * scale;
                let oy = 8.0 + row as f64 * scale;
                for dy in 0..scale as i32 {
                    for dx in 0..scale as i32 {
                        plot(ox + f64::from(dx), oy + f64::from(dy), colour);
                    }
                }
            }
        }
    }
}

/// Fills the whole buffer: a background that cycles colour, a sweeping bar, and
/// a grid so a frozen region is obvious against a moving one.
fn draw(
    file: &File,
    frame: u32,
    transform: Transform,
    src: Option<(f64, f64, f64, f64)>,
    last: Transform,
    now: Transform,
) {
    let w = BUFFER_W as usize;
    let h = BUFFER_H as usize;
    let mut pixels = vec![0u8; w * h * 4];

    let phase = frame as usize;
    let bar = (phase * 13) % w;

    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;

            // Diagonal bands that march across the surface every frame.
            let band = ((x + y + phase * 7) / 40).is_multiple_of(2);
            let (mut r, mut g, mut b) = if band {
                (30u32, 30u32, 40u32)
            } else {
                (200, 60, 90)
            };

            // A grid, so a stale region is legible even while static.
            if x % 100 == 0 || y % 100 == 0 {
                r = 255;
                g = 255;
                b = 255;
            }

            // A bright bar sweeping left to right.
            if x.abs_diff(bar) < 8 {
                r = 255;
                g = 220;
                b = 0;
            }

            // Argb8888 is little endian: b, g, r, a.
            pixels[i] = b as u8;
            pixels[i + 1] = g as u8;
            pixels[i + 2] = r as u8;
            pixels[i + 3] = 255;
        }
    }

    draw_indicator(&mut pixels, transform, src, last, now);

    file.write_at(&pixels, 0).expect("write pixels");
}

fn main() {
    let mode_name = env::args().nth(1).unwrap_or_else(|| "dst".to_string());
    let mode = match mode_name.as_str() {
        "dst" => Mode {
            transform: Transform::Normal,
            src: None,
            rotate_every: None,
            buffer_damage: false,
        },
        "src180" => Mode {
            transform: Transform::_180,
            // Crop a little so the source offset participates in the mapping.
            src: Some((20.0, 20.0, 560.0, 360.0)),
            rotate_every: None,
            buffer_damage: false,
        },
        "rotate" => Mode {
            transform: Transform::Normal,
            src: Some((20.0, 20.0, 560.0, 360.0)),
            rotate_every: Some(90),
            buffer_damage: false,
        },
        // tN pins one transform, so a frozen window is deterministic rather
        // than a 1.5s window inside the rotation.
        other if other.starts_with('t') || other.starts_with('b') => {
            let index: usize = other[1..]
                .parse()
                .unwrap_or_else(|_| panic!("expected t0..t7, got {other:?}"));
            assert!(index < TRANSFORMS.len(), "expected t0..t7, got {other:?}");
            Mode {
                transform: TRANSFORMS[index],
                src: Some((20.0, 20.0, 560.0, 360.0)),
                rotate_every: None,
                buffer_damage: other.starts_with('b'),
            }
        },
        other => panic!("unknown mode {other:?}, expected dst, src180, rotate or t0..t7"),
    };

    let conn = Connection::connect_to_env().expect("connect to wayland display");
    let (globals, mut queue) = registry_queue_init::<State>(&conn).expect("registry init");
    let qh = queue.handle();

    let compositor: WlCompositor = globals.bind(&qh, 1..=6, ()).expect("wl_compositor");
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).expect("wl_shm");
    let wm_base: XdgWmBase = globals.bind(&qh, 1..=5, ()).expect("xdg_wm_base");
    let viewporter: WpViewporter = globals.bind(&qh, 1..=1, ()).expect("wp_viewporter");

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title(format!("wprs damage demo [{mode_name}]"));
    surface.commit();

    let mut state = State {
        configured: false,
        frame_done: true,
        closed: false,
    };
    while !state.configured {
        queue.blocking_dispatch(&mut state).expect("dispatch");
    }

    let len = (BUFFER_W * BUFFER_H * 4) as u64;
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open("/tmp/wprs-damage-demo.buf")
        .expect("backing file");
    file.set_len(len).expect("size backing file");
    let pool = shm.create_pool(file.as_fd(), len as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        BUFFER_W,
        BUFFER_H,
        BUFFER_W * 4,
        Format::Argb8888,
        &qh,
        (),
    );

    surface.set_buffer_transform(mode.transform);
    let viewport = viewporter.get_viewport(&surface, &qh, ());
    if let Some((x, y, w, h)) = mode.src {
        viewport.set_source(x, y, w, h);
    }
    viewport.set_destination(SURFACE_W, SURFACE_H);

    println!(
        "mode {mode_name}: buffer {BUFFER_W}x{BUFFER_H}, surface {SURFACE_W}x{SURFACE_H}, \
         transform {:?}, src {:?}",
        mode.transform, mode.src
    );
    if mode.buffer_damage {
        println!("damage: whole buffer, in BUFFER coordinates (wprsd conversion bypassed)");
    } else {
        println!("damage: whole surface, in SURFACE coordinates (wprsd converts it)");
    }

    let mut frame: u32 = 0;
    let mut transform_index = 0;
    let mut current = mode.transform;
    let mut previous = mode.transform;
    while !state.closed {
        if !state.frame_done {
            queue.blocking_dispatch(&mut state).expect("dispatch");
            continue;
        }
        state.frame_done = false;

        if let Some(every) = mode.rotate_every
            && frame.is_multiple_of(every)
        {
            previous = current;
            current = TRANSFORMS[transform_index % TRANSFORMS.len()];
            transform_index += 1;
            surface.set_buffer_transform(current);
            println!("frame {frame}: buffer_transform {previous:?} -> {current:?}");
        }

        draw(&file, frame, current, mode.src, previous, current);
        frame += 1;

        surface.attach(Some(&buffer), 0, 0);
        if mode.buffer_damage {
            // Buffer coordinates, the whole buffer. wprsd forwards these
            // untouched, so this isolates the conversion from everything else.
            surface.damage_buffer(0, 0, BUFFER_W, BUFFER_H);
        } else {
            // Surface coordinates, covering everything. A correct conversion
            // turns this into the whole buffer.
            surface.damage(0, 0, SURFACE_W, SURFACE_H);
        }
        surface.frame(&qh, ());
        surface.commit();

        queue.flush().expect("flush");
    }
}
