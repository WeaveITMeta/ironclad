//! Screen capture via `getDisplayMedia`.
//!
//! Two modes:
//! - **One-shot** (`capture_screen`): pick a display, grab one frame,
//!   immediately release the stream. Used by the explicit "look" button.
//! - **Ambient stream** (`start_screen_stream` / `grab_stream_frame` /
//!   `stop_screen_stream`): pick the display once, keep the stream alive,
//!   draw a fresh frame onto a canvas every time a voice turn dispatches.
//!   This is what makes JARVIS "see what I'm looking at" without
//!   re-prompting on every utterance.
//!
//! Both modes return base64 PNG with the `data:image/png;base64,` prefix
//! stripped, matching what the Anthropic provider expects on the wire.

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    DisplayMediaStreamConstraints, HtmlCanvasElement, HtmlVideoElement, MediaStream,
    MediaStreamTrack,
};

const STREAM_VIDEO_ID: &str = "__jarvis_screen_video";

/// One-shot grab. User picks a display each time; stream is released
/// immediately after the single frame lands. Kept around for explicit
/// "show JARVIS this one thing right now" use.
pub async fn capture_screen() -> Result<String, String> {
    let stream = request_display_stream().await?;
    let video = build_video_for(&stream).await?;
    let result = draw_video_to_base64(&video);
    stop_tracks(&stream);
    result
}

/// Open the OS picker, mount the resulting MediaStream onto a hidden
/// `<video>` in the document so we can keep grabbing frames from it. Stows
/// the stream on `window.__jarvis_screen_stream` so callers can stop it
/// later without rebuilding any Rust-side state.
pub async fn start_screen_stream() -> Result<(), String> {
    // Idempotent: if a stream is already active, do nothing.
    if is_stream_active() {
        return Ok(());
    }
    let stream = request_display_stream().await?;
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;

    // Re-use a single hidden <video> element so frame grabs land on the
    // same surface across the session.
    let video: HtmlVideoElement = match document.get_element_by_id(STREAM_VIDEO_ID) {
        Some(el) => el
            .dyn_into()
            .map_err(|_| "existing screen video wrong type".to_string())?,
        None => {
            let v: HtmlVideoElement = document
                .create_element("video")
                .map_err(|e| format!("create video: {:?}", e))?
                .dyn_into()
                .map_err(|_| "not a video".to_string())?;
            v.set_id(STREAM_VIDEO_ID);
            v.set_autoplay(true);
            v.set_muted(true);
            v.set_attribute("playsinline", "")
                .map_err(|e| format!("playsinline: {:?}", e))?;
            v.style().set_property("position", "fixed").ok();
            v.style().set_property("left", "-9999px").ok();
            v.style().set_property("top", "-9999px").ok();
            v.style().set_property("width", "2px").ok();
            v.style().set_property("height", "2px").ok();
            if let Some(body) = document.body() {
                body.append_child(&v)
                    .map_err(|e| format!("append video: {:?}", e))?;
            }
            v
        }
    };
    video.set_src_object(Some(&stream));

    // Wait for metadata + one animation frame so videoWidth / videoHeight
    // are populated by the time the first frame-grab tries to use them.
    let metadata_promise = once_event(&video, "loadedmetadata");
    let _ = JsFuture::from(metadata_promise).await;
    let _ = JsFuture::from(animation_frame()).await;

    // Stash the stream on window so stop_screen_stream() can find it.
    let win_js: JsValue = window.into();
    js_sys::Reflect::set(
        &win_js,
        &JsValue::from_str("__jarvis_screen_stream"),
        stream.as_ref(),
    )
    .map_err(|e| format!("stash stream: {:?}", e))?;

    // Also wire an `ended` listener: if the user clicks Chrome's "Stop
    // sharing" pill, we tear down our state cleanly instead of holding a
    // dead reference.
    if let Some(track) = stream
        .get_video_tracks()
        .get(0)
        .dyn_into::<MediaStreamTrack>()
        .ok()
    {
        let cb = wasm_bindgen::closure::Closure::once_into_js(move || {
            stop_screen_stream();
        });
        track
            .add_event_listener_with_callback("ended", cb.as_ref().unchecked_ref())
            .ok();
    }

    Ok(())
}

/// Pull a fresh frame off the live screen-share stream. Returns base64 PNG
/// without the `data:` prefix, or `None` if no stream is active (so the
/// per-turn auto-attach can be silent rather than noisy).
pub fn grab_stream_frame() -> Option<String> {
    let window = web_sys::window()?;
    let document = window.document()?;
    let video: HtmlVideoElement = document
        .get_element_by_id(STREAM_VIDEO_ID)?
        .dyn_into()
        .ok()?;
    if !is_stream_active() {
        return None;
    }
    if video.video_width() == 0 || video.video_height() == 0 {
        return None;
    }
    draw_video_to_base64(&video).ok()
}

/// Tear down the active screen-share stream. Safe to call multiple times.
pub fn stop_screen_stream() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let win_js: JsValue = window.clone().into();
    if let Ok(stream_js) =
        js_sys::Reflect::get(&win_js, &JsValue::from_str("__jarvis_screen_stream"))
    {
        if let Ok(stream) = stream_js.dyn_into::<MediaStream>() {
            stop_tracks(&stream);
        }
    }
    let _ = js_sys::Reflect::delete_property(
        &win_js.dyn_into::<js_sys::Object>().unwrap_or_default(),
        &JsValue::from_str("__jarvis_screen_stream"),
    );
    if let Some(doc) = window.document() {
        if let Some(video) = doc.get_element_by_id(STREAM_VIDEO_ID) {
            if let Ok(v) = video.dyn_into::<HtmlVideoElement>() {
                v.set_src_object(None);
            }
        }
    }
}

/// True iff the user currently has an active screen-share stream.
pub fn is_stream_active() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let win_js: JsValue = window.into();
    match js_sys::Reflect::get(&win_js, &JsValue::from_str("__jarvis_screen_stream")) {
        Ok(v) if v.is_truthy() => {
            // Also check the tracks are still live; Chrome can end the
            // stream from the "Stop sharing" pill without us hearing.
            if let Ok(stream) = v.dyn_into::<MediaStream>() {
                let tracks = stream.get_video_tracks();
                for i in 0..tracks.length() {
                    if let Ok(track) = tracks.get(i).dyn_into::<MediaStreamTrack>() {
                        if track.ready_state() == web_sys::MediaStreamTrackState::Live {
                            return true;
                        }
                    }
                }
            }
            false
        }
        _ => false,
    }
}

// --- Helpers ----------------------------------------------------------------

async fn request_display_stream() -> Result<MediaStream, String> {
    let window = web_sys::window().ok_or("no window")?;
    let nav = window.navigator();
    let media = nav
        .media_devices()
        .map_err(|_| "MediaDevices unavailable".to_string())?;
    let constraints = DisplayMediaStreamConstraints::new();
    constraints.set_video(&JsValue::TRUE);
    constraints.set_audio(&JsValue::FALSE);
    let promise = media
        .get_display_media_with_constraints(&constraints)
        .map_err(|e| format!("getDisplayMedia: {:?}", e))?;
    let stream_js = JsFuture::from(promise)
        .await
        .map_err(|e| format!("getDisplayMedia await: {:?}", e))?;
    stream_js
        .dyn_into()
        .map_err(|_| "not a MediaStream".to_string())
}

async fn build_video_for(stream: &MediaStream) -> Result<HtmlVideoElement, String> {
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let video: HtmlVideoElement = document
        .create_element("video")
        .map_err(|e| format!("create video: {:?}", e))?
        .dyn_into()
        .map_err(|_| "not a video element".to_string())?;
    video.set_autoplay(true);
    video.set_muted(true);
    video.set_src_object(Some(stream));
    let _ = JsFuture::from(once_event(&video, "loadedmetadata")).await;
    let _ = JsFuture::from(animation_frame()).await;
    Ok(video)
}

/// Anthropic's recommended max long edge for vision images. Going larger
/// wastes input tokens without improving comprehension; going smaller
/// (e.g. 1024) saves tokens at a noticeable quality cost for dense UIs.
/// Combined with JPEG q=0.85 this keeps a full-screen capture well
/// under Anthropic's 5 MB per-image hard limit on any monitor McKale
/// owns (4K source → ~200-500 KB JPEG).
const MAX_IMAGE_LONG_EDGE: i32 = 1568;
const JPEG_QUALITY: f64 = 0.85;

fn draw_video_to_base64(video: &HtmlVideoElement) -> Result<String, String> {
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let src_w = video.video_width() as i32;
    let src_h = video.video_height() as i32;
    if src_w <= 0 || src_h <= 0 {
        return Err("video metadata not ready".to_string());
    }

    // Scale so the longest edge fits MAX_IMAGE_LONG_EDGE, preserving
    // aspect ratio. Source already-small images stay 1:1 (we don't
    // upscale — that just inflates bytes for no quality gain).
    let (dst_w, dst_h) = scale_to_max_edge(src_w, src_h, MAX_IMAGE_LONG_EDGE);

    let canvas: HtmlCanvasElement = document
        .create_element("canvas")
        .map_err(|e| format!("create canvas: {:?}", e))?
        .dyn_into()
        .map_err(|_| "not a canvas".to_string())?;
    canvas.set_width(dst_w as u32);
    canvas.set_height(dst_h as u32);
    let ctx = canvas
        .get_context("2d")
        .map_err(|e| format!("getContext: {:?}", e))?
        .ok_or("2d context unavailable")?
        .dyn_into::<web_sys::CanvasRenderingContext2d>()
        .map_err(|_| "not a 2d context".to_string())?;
    // High-quality downscale: imageSmoothingEnabled + high quality.
    // Defaults vary by browser; force the good path.
    let _ = js_sys::Reflect::set(
        ctx.as_ref(),
        &JsValue::from_str("imageSmoothingEnabled"),
        &JsValue::TRUE,
    );
    let _ = js_sys::Reflect::set(
        ctx.as_ref(),
        &JsValue::from_str("imageSmoothingQuality"),
        &JsValue::from_str("high"),
    );
    ctx.draw_image_with_html_video_element_and_dw_and_dh(
        video,
        0.0,
        0.0,
        dst_w as f64,
        dst_h as f64,
    )
    .map_err(|e| format!("drawImage: {:?}", e))?;
    // JPEG instead of PNG. Screenshots are photographic enough that JPEG
    // is the right choice — 10x smaller than PNG for the same visual
    // fidelity at q=0.85. PNG was the source of the silent-failure mode
    // where screen-share frames pushed the request body past Anthropic's
    // 5 MB per-image limit and the entire turn rejected.
    let data_url = canvas
        .to_data_url_with_type_and_encoder_options(
            "image/jpeg",
            &JsValue::from_f64(JPEG_QUALITY),
        )
        .map_err(|e| format!("toDataURL: {:?}", e))?;
    let comma = data_url
        .find(',')
        .ok_or("data URL had no comma separator".to_string())?;
    let b64 = &data_url[comma + 1..];
    // Visible-in-devtools log so we can confirm sizes once McKale shares
    // his screen again. Format: "[screen] 1568x882 jpeg=482KB" or similar.
    let approx_bytes = (b64.len() * 3) / 4;
    web_sys::console::log_1(
        &format!(
            "[screen] {}x{} jpeg ~ {}KB (src {}x{})",
            dst_w,
            dst_h,
            approx_bytes / 1024,
            src_w,
            src_h
        )
        .into(),
    );
    Ok(b64.to_string())
}

/// Preserve aspect ratio while capping the longer edge at `max`. Never
/// upscales (so a 1024x768 input stays 1024x768 even if max is 1568).
fn scale_to_max_edge(src_w: i32, src_h: i32, max: i32) -> (i32, i32) {
    let long = src_w.max(src_h);
    if long <= max {
        return (src_w, src_h);
    }
    let scale = max as f64 / long as f64;
    let dst_w = ((src_w as f64) * scale).round().max(1.0) as i32;
    let dst_h = ((src_h as f64) * scale).round().max(1.0) as i32;
    (dst_w, dst_h)
}

fn once_event(target: &web_sys::EventTarget, name: &'static str) -> js_sys::Promise {
    let target = target.clone();
    js_sys::Promise::new(&mut move |resolve, _reject| {
        let cb = wasm_bindgen::closure::Closure::once_into_js(move || {
            let _ = resolve.call0(&JsValue::NULL);
        });
        target
            .add_event_listener_with_callback(name, cb.as_ref().unchecked_ref())
            .unwrap_or(());
    })
}

fn animation_frame() -> js_sys::Promise {
    js_sys::Promise::new(&mut |resolve, _reject| {
        let w = web_sys::window().expect("window");
        let cb = wasm_bindgen::closure::Closure::once_into_js(move |_: JsValue| {
            let _ = resolve.call0(&JsValue::NULL);
        });
        let _ = w.request_animation_frame(cb.as_ref().unchecked_ref());
    })
}

fn stop_tracks(stream: &MediaStream) {
    let tracks = stream.get_tracks();
    for i in 0..tracks.length() {
        if let Ok(track) = tracks.get(i).dyn_into::<MediaStreamTrack>() {
            track.stop();
        }
    }
}
