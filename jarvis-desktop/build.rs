//! Build script.
//!
//! Two responsibilities:
//!   1. Compile every .slint file in ui/ via slint-build.
//!   2. Rasterize the arc-reactor SVG to a multi-resolution .ico (256,
//!      128, 64, 48, 32, 16) and embed it as the .exe's icon resource
//!      via winres. This is what populates the Windows taskbar,
//!      Alt-Tab list, Explorer file icon, and window title bar — the
//!      runtime `slint::Image` set on the Window's `icon` property only
//!      covers the in-app surfaces, not the OS chrome.
//!
//! Mirrors the pattern Eustress Engine uses for its desktop icon.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

fn main() {
    // ---- Slint UI compilation ----
    slint_build::compile("ui/main.slint").expect("slint compile main.slint");

    // ---- ICO generation ----
    let svg_path = Path::new("assets/jarvis.svg");
    let ico_path = Path::new("assets/jarvis.ico");
    println!("cargo:rerun-if-changed=assets/jarvis.svg");
    println!("cargo:rerun-if-changed=build.rs");

    if !svg_path.exists() {
        panic!("missing assets/jarvis.svg — desktop icon source");
    }

    let svg_modified = std::fs::metadata(svg_path).unwrap().modified().unwrap();
    let needs_rebuild = !ico_path.exists()
        || {
            let ico_modified = std::fs::metadata(ico_path).unwrap().modified().unwrap();
            svg_modified > ico_modified
        };

    if needs_rebuild {
        let svg_data = std::fs::read(svg_path).expect("read jarvis.svg");
        let tree = usvg::Tree::from_data(&svg_data, &usvg::Options::default())
            .expect("parse jarvis.svg");

        // Windows ICO supports up to 256×256 per frame. Multiple sizes
        // let Windows pick the right one per surface (16 in some
        // notification flyouts, 32 for taskbar small, 256 for the
        // Alt-Tab thumbnail at high DPI).
        let sizes = [256u32, 128, 64, 48, 32, 16];
        let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);

        let src_long_edge = tree.size().width().max(tree.size().height());
        for size in sizes {
            let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("alloc pixmap");
            let scale = size as f32 / src_long_edge;
            let transform = tiny_skia::Transform::from_scale(scale, scale);
            resvg::render(&tree, transform, &mut pixmap.as_mut());

            // tiny-skia gives us RGBA; ico expects RGBA too (despite the
            // file format storing BGRA internally — `from_rgba_data`
            // does the conversion for us in current `ico` versions).
            let rgba = pixmap.take();
            let image = ico::IconImage::from_rgba_data(size, size, rgba);
            icon_dir.add_entry(
                ico::IconDirEntry::encode(&image).expect("encode ICO frame"),
            );
        }

        let file = File::create(ico_path).expect("create jarvis.ico");
        icon_dir
            .write(BufWriter::new(file))
            .expect("write jarvis.ico");
        println!("cargo:warning=✅ jarvis.ico rebuilt at {} sizes", sizes.len());
    }

    // ---- Windows resource embedding ----
    #[cfg(target_os = "windows")]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/jarvis.ico");
        res.set("OriginalFilename", "jarvis-desktop.exe");
        res.set("FileDescription", "JARVIS native desktop overlay");
        res.set("ProductName", "JARVIS");
        res.set("LegalCopyright", "Copyright © 2026 McKale Olson");
        if let Err(e) = res.compile() {
            // Don't kill the build on Windows resource embedding errors;
            // the binary still runs, the taskbar just stays generic.
            // Warn loudly so we know to fix it.
            println!("cargo:warning=❌ icon resource embedding failed: {e}");
        }
    }
}
