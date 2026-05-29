//! Arc-reactor icon rasterization.
//!
//! The same `jarvis.svg` ships as the favicon for the Leptos dashboard,
//! the tray icon, the title-bar icon, and the centerpiece widget. This
//! module rasterizes it at whatever size the consumer needs via resvg +
//! tiny-skia. Compile-time `include_bytes!` keeps the binary self-
//! contained (no runtime asset lookup, no path-resolution surprises).

use tray_icon::Icon;

/// Embedded SVG bytes for the arc-reactor icon. Same source the Leptos
/// dashboard uses as `favicon.svg`; embedded so the desktop overlay
/// stays a single self-contained binary.
const JARVIS_SVG: &[u8] = include_bytes!("../assets/jarvis.svg");

/// Small UI icons (compile-time embedded so the binary stays
/// self-contained). All three use `currentColor` strokes so we can
/// retint them at rasterization time.
const GEAR_SVG: &[u8] = include_bytes!("../assets/icons/gear.svg");
const MINIFY_SVG: &[u8] = include_bytes!("../assets/icons/minify.svg");
const MONITOR_SVG: &[u8] = include_bytes!("../assets/icons/monitor.svg");

/// Rasterize the arc-reactor SVG at the given square size, returning
/// the row-major RGBA byte buffer.
pub fn rasterize_jarvis_icon(size: u32) -> Vec<u8> {
    rasterize_svg(JARVIS_SVG, size)
}

fn rasterize_svg(svg: &[u8], size: u32) -> Vec<u8> {
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_data(svg, &opt).expect("parse SVG asset");
    let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("alloc pixmap");
    let svg_size = tree.size();
    let scale_x = size as f32 / svg_size.width();
    let scale_y = size as f32 / svg_size.height();
    let transform = tiny_skia::Transform::from_scale(scale_x, scale_y);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    pixmap.data().to_vec()
}

fn svg_to_slint_image(svg: &[u8], size: u32) -> slint::Image {
    let rgba = rasterize_svg(svg, size);
    let buffer =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, size, size);
    slint::Image::from_rgba8(buffer)
}

/// Settings gear icon — outline cog with filled center hub.
pub fn build_gear_icon() -> slint::Image {
    svg_to_slint_image(GEAR_SVG, 48)
}

/// Minify icon — four corner brackets pulling inward.
pub fn build_minify_icon() -> slint::Image {
    svg_to_slint_image(MINIFY_SVG, 56)
}

/// Screen-share icon — desktop monitor with signal arcs.
pub fn build_monitor_icon() -> slint::Image {
    svg_to_slint_image(MONITOR_SVG, 56)
}

/// Build the tray icon from the SVG at 32×32 (Windows tray standard).
pub fn build_tray_icon() -> Icon {
    let rgba = rasterize_jarvis_icon(32);
    Icon::from_rgba(rgba, 32, 32).expect("tray icon from rasterized SVG")
}

/// Build a Slint Image from the SVG at 256×256 for the window icon.
/// Windows uses 256 for the high-DPI title-bar / Alt-Tab thumbnail;
/// the OS scales down for the smaller renderings.
pub fn build_window_icon_image() -> slint::Image {
    let size = 256u32;
    let rgba = rasterize_jarvis_icon(size);
    let buffer =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, size, size);
    slint::Image::from_rgba8(buffer)
}

/// Larger render used as the centerpiece of the floating widget AND
/// the full HUD. 384×384 gives Slint enough pixels to scale smoothly
/// at the largest display size (168×168 in the HUD).
pub fn build_reactor_image() -> slint::Image {
    let size = 384u32;
    let rgba = rasterize_jarvis_icon(size);
    let buffer =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, size, size);
    slint::Image::from_rgba8(buffer)
}
