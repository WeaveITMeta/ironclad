//! Picture-in-Picture pin. Chrome aggressively throttles AudioContext,
//! AudioWorklet, and `getUserMedia` once a tab loses visibility (audio
//! capture often stops entirely after ~1 minute backgrounded). Pinning
//! a tiny PiP video keeps the source tab marked as "user-attended,"
//! preventing the suspend.
//!
//! Mechanism: build a 2x2 canvas, drive it as a video stream via
//! `captureStream(1)`, point a hidden <video> at it, request PiP. The
//! PiP overlay floats above other windows; while it's open, the dashboard
//! tab's audio pipeline keeps running even when the JARVIS tab is hidden.

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{HtmlCanvasElement, HtmlVideoElement};

const CANVAS_ID: &str = "__jarvis_pip_canvas";
const VIDEO_ID: &str = "__jarvis_pip_video";

/// Open a Picture-in-Picture floating window so the dashboard's voice
/// pipeline survives the tab going to background. Idempotent: calling it
/// while PiP is already open is a no-op.
pub async fn pin_pip() -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;

    // Already in PiP? Then nothing to do.
    if let Ok(pip_el) = js_sys::Reflect::get(&document, &JsValue::from_str("pictureInPictureElement"))
    {
        if !pip_el.is_null() && !pip_el.is_undefined() {
            return Ok(());
        }
    }

    // Re-use the canvas/video pair across multiple pin/unpin cycles. If
    // they don't exist yet, build them once and stash them in the DOM.
    let canvas: HtmlCanvasElement = match document.get_element_by_id(CANVAS_ID) {
        Some(el) => el
            .dyn_into()
            .map_err(|_| "existing canvas wrong type".to_string())?,
        None => {
            let c: HtmlCanvasElement = document
                .create_element("canvas")
                .map_err(|e| format!("create canvas: {:?}", e))?
                .dyn_into()
                .map_err(|_| "not a canvas".to_string())?;
            c.set_id(CANVAS_ID);
            // 240x240 is big enough to look sharp in Chrome's PiP overlay
            // (~300px wide by default) and small enough for the
            // requestAnimationFrame loop in pip-animation.js to stay
            // under 1% CPU. The flat 2x2 cyan square it replaced looked
            // unfinished; this gives the PiP video an arc-reactor pulse
            // animation that matches the dashboard aesthetic.
            c.set_width(240);
            c.set_height(240);
            // Off-screen but in the DOM tree (required for captureStream).
            c.style().set_property("position", "fixed").ok();
            c.style().set_property("left", "-9999px").ok();
            c.style().set_property("top", "-9999px").ok();
            if let Some(body) = document.body() {
                body.append_child(&c)
                    .map_err(|e| format!("append canvas: {:?}", e))?;
            }
            // Initial paint so the captureStream has SOMETHING before the
            // animation loop kicks in. The JS animation overwrites this.
            if let Some(Ok(ctx)) = c.get_context("2d").ok().and_then(|opt| {
                opt.map(|o| o.dyn_into::<web_sys::CanvasRenderingContext2d>())
            }) {
                ctx.set_fill_style_str("#020617");
                ctx.fill_rect(0.0, 0.0, 240.0, 240.0);
            }
            c
        }
    };

    let video: HtmlVideoElement = match document.get_element_by_id(VIDEO_ID) {
        Some(el) => el
            .dyn_into()
            .map_err(|_| "existing video wrong type".to_string())?,
        None => {
            let v: HtmlVideoElement = document
                .create_element("video")
                .map_err(|e| format!("create video: {:?}", e))?
                .dyn_into()
                .map_err(|_| "not a video".to_string())?;
            v.set_id(VIDEO_ID);
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

    // Kick the arc-reactor animation loop. Defined in pip-animation.js
    // and exposed on window as `__startPipAnimation(canvasId)`. Idempotent
    // on the JS side, so re-pinning after an unpin restarts cleanly.
    if let Some(win) = web_sys::window() {
        if let Ok(start_fn) =
            js_sys::Reflect::get(&win, &JsValue::from_str("__startPipAnimation"))
        {
            if let Ok(f) = start_fn.dyn_into::<js_sys::Function>() {
                let _ = f.call1(&win, &JsValue::from_str(CANVAS_ID));
            }
        }
    }

    // captureStream isn't on HtmlCanvasElement's typed surface; reach for
    // it via Reflect.
    let capture_fn = js_sys::Reflect::get(&canvas, &JsValue::from_str("captureStream"))
        .map_err(|e| format!("captureStream lookup: {:?}", e))?;
    let capture_fn: js_sys::Function = capture_fn
        .dyn_into()
        .map_err(|_| "captureStream not a function".to_string())?;
    // 24 fps capture: cinematic feel, lower CPU than 30, smooth enough
    // for the arc-reactor pulse to read as motion in the PiP overlay.
    let stream = capture_fn
        .call1(&canvas, &JsValue::from_f64(24.0))
        .map_err(|e| format!("captureStream call: {:?}", e))?;
    let stream: web_sys::MediaStream = stream
        .dyn_into()
        .map_err(|_| "captureStream returned wrong type".to_string())?;
    video.set_src_object(Some(&stream));

    // Chrome rejects requestPictureInPicture if videoWidth/videoHeight
    // aren't populated yet. set_src_object doesn't synchronously load the
    // first frame's metadata — we have to wait for `loadedmetadata` to
    // fire (or check `readyState >= HAVE_METADATA` if it's already past).
    if video.ready_state() < 1 {
        let v = video.clone();
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            let cb = wasm_bindgen::closure::Closure::once_into_js(
                move |_: JsValue| {
                    let _ = resolve.call0(&JsValue::NULL);
                },
            );
            v.add_event_listener_with_callback(
                "loadedmetadata",
                cb.as_ref().unchecked_ref(),
            )
            .ok();
        });
        let _ = JsFuture::from(promise).await;
    }

    // Now that metadata is loaded, kick playback. Some browsers also
    // need a frame to actually paint before PiP will attach, so we await
    // one rAF after play() returns.
    let play_promise = video
        .play()
        .map_err(|e| format!("video.play(): {:?}", e))?;
    let _ = JsFuture::from(play_promise).await;
    {
        let raf_promise = js_sys::Promise::new(&mut |resolve, _reject| {
            if let Some(w) = web_sys::window() {
                let cb = wasm_bindgen::closure::Closure::once_into_js(
                    move |_: JsValue| {
                        let _ = resolve.call0(&JsValue::NULL);
                    },
                );
                let _ = w.request_animation_frame(cb.as_ref().unchecked_ref());
            }
        });
        let _ = JsFuture::from(raf_promise).await;
    }

    // Defensive: confirm metadata actually populated before asking Chrome.
    if video.video_width() == 0 || video.video_height() == 0 {
        return Err(format!(
            "video metadata still empty after wait (readyState={}); skipping PiP",
            video.ready_state()
        ));
    }

    // Request PiP. This is the call that surfaces the floating window.
    let req = js_sys::Reflect::get(&video, &JsValue::from_str("requestPictureInPicture"))
        .map_err(|e| format!("requestPictureInPicture lookup: {:?}", e))?;
    let req: js_sys::Function = req
        .dyn_into()
        .map_err(|_| "requestPictureInPicture not a function".to_string())?;
    let pip_promise = req
        .call0(&video)
        .map_err(|e| format!("requestPictureInPicture call: {:?}", e))?;
    let pip_promise: js_sys::Promise = pip_promise
        .dyn_into()
        .map_err(|_| "requestPictureInPicture returned non-Promise".to_string())?;
    JsFuture::from(pip_promise)
        .await
        .map_err(|e| format!("requestPictureInPicture rejected: {:?}", e))?;

    Ok(())
}

/// Install a Page Visibility listener that auto-pins PiP whenever the tab
/// loses focus (user alt-tabs, switches browser tabs, minimizes the
/// window), and auto-unpins when focus returns. Must be called once after
/// the user has made a gesture (mic open is fine) so the browser allows
/// the PiP request — once primed, subsequent visibility-change pins fire
/// without further user interaction.
///
/// Auto-pin won't trip if the user manually pinned earlier (we only undo
/// what we initiated). Safe to call multiple times; the listener attaches
/// once via a window-level flag.
pub fn install_auto_pip(on_pinned: impl Fn(bool) + 'static) {
    let window = match web_sys::window() {
        Some(w) => w,
        None => return,
    };
    let document = match window.document() {
        Some(d) => d,
        None => return,
    };
    // Guard: only install once per session.
    let win_js: JsValue = window.clone().into();
    if let Ok(flag) = js_sys::Reflect::get(&win_js, &JsValue::from_str("__jarvis_auto_pip_installed")) {
        if flag.is_truthy() {
            return;
        }
    }
    let _ = js_sys::Reflect::set(
        &win_js,
        &JsValue::from_str("__jarvis_auto_pip_installed"),
        &JsValue::TRUE,
    );

    let cb_on_pinned = std::rc::Rc::new(on_pinned);
    let cb_on_pinned_for_handler = cb_on_pinned.clone();
    let closure = wasm_bindgen::closure::Closure::wrap(Box::new(move || {
        let Some(window) = web_sys::window() else { return };
        let Some(document) = window.document() else { return };
        let hidden = document.hidden();
        let win_js: JsValue = window.clone().into();
        let auto_pinned = js_sys::Reflect::get(&win_js, &JsValue::from_str("__jarvis_auto_pinned"))
            .map(|v| v.is_truthy())
            .unwrap_or(false);
        let cb = cb_on_pinned_for_handler.clone();
        if hidden {
            // Tab just went to background: pin so audio stays alive.
            wasm_bindgen_futures::spawn_local(async move {
                match pin_pip().await {
                    Ok(()) => {
                        if let Some(w) = web_sys::window() {
                            let _ = js_sys::Reflect::set(
                                &w.into(),
                                &JsValue::from_str("__jarvis_auto_pinned"),
                                &JsValue::TRUE,
                            );
                        }
                        (cb)(true);
                    }
                    Err(e) => {
                        web_sys::console::log_1(
                            &format!("[pip] auto-pin failed: {e}").into(),
                        );
                    }
                }
            });
        } else if auto_pinned {
            // Tab came back AND we were the ones who auto-pinned — release.
            wasm_bindgen_futures::spawn_local(async move {
                let _ = unpin_pip().await;
                if let Some(w) = web_sys::window() {
                    let _ = js_sys::Reflect::set(
                        &w.into(),
                        &JsValue::from_str("__jarvis_auto_pinned"),
                        &JsValue::FALSE,
                    );
                }
                (cb)(false);
            });
        }
    }) as Box<dyn Fn()>);

    document
        .add_event_listener_with_callback("visibilitychange", closure.as_ref().unchecked_ref())
        .ok();
    closure.forget();
}

/// Close the PiP overlay if one is open. No-op otherwise.
pub async fn unpin_pip() -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;

    // Stop the arc-reactor animation loop so we don't burn CPU drawing
    // into a canvas nobody's looking at anymore.
    if let Ok(stop_fn) =
        js_sys::Reflect::get(&window, &JsValue::from_str("__stopPipAnimation"))
    {
        if let Ok(f) = stop_fn.dyn_into::<js_sys::Function>() {
            let _ = f.call0(&window);
        }
    }

    let exit = js_sys::Reflect::get(&document, &JsValue::from_str("exitPictureInPicture"))
        .map_err(|e| format!("exitPictureInPicture lookup: {:?}", e))?;
    if exit.is_undefined() || exit.is_null() {
        return Ok(());
    }
    let exit: js_sys::Function = exit
        .dyn_into()
        .map_err(|_| "exitPictureInPicture not a function".to_string())?;
    let promise = exit
        .call0(&document)
        .map_err(|e| format!("exitPictureInPicture call: {:?}", e))?;
    if let Ok(p) = promise.dyn_into::<js_sys::Promise>() {
        let _ = JsFuture::from(p).await;
    }
    Ok(())
}
