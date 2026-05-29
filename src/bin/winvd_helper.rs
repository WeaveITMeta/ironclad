//! Subprocess wrapper around the `winvd` crate's Virtual Desktop COM
//! calls. Isolated from the main gateway so winvd's recurring 0xC0000005
//! access violations (the COM interface IDs shift between Windows 11
//! builds and the crate occasionally lags) only kill THIS process, not
//! every Playwright instance, the autonomous loop, voice, and the chat
//! agent simultaneously.
//!
//! Protocol: CLI args in, JSON on stdout, errors on stderr, exit code
//! 0 = success, non-zero = error or SEH crash.
//!
//!   winvd_helper list                          → {count, current_idx, names: [...]}
//!   winvd_helper switch <idx>                  → {ok: true}
//!   winvd_helper create [<name>]               → {idx, name}
//!   winvd_helper move <hwnd_decimal> <idx>     → {ok: true}
//!
//! On non-Windows builds the binary still compiles (as a stub) so the
//! workspace builds on Linux/macOS CI; calling it there exits with a
//! "not supported" message.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: winvd_helper <list|switch|create|move> [args...]");
        return ExitCode::from(2);
    }
    let op = args[1].as_str();

    // winvd's COM calls require an initialized apartment. Its internal
    // code uses `windows::Win32::System::Com::CoInitializeEx` with
    // `COINIT_MULTITHREADED` (MTA) — using STA causes IVirtualDesktop
    // vtable calls to segfault on cross-thread marshalling. The parent
    // ironclad process gets MTA as a side effect of other infrastructure
    // (reqwest's tokio threadpool, etc.); standalone we must do it
    // ourselves before any winvd call. Uses windows 0.58 (matching
    // winvd 0.0.49's expectation), separate from the rest of the
    // project which is still on windows 0.52.
    #[cfg(target_os = "windows")]
    {
        use windows_58::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
    }

    let result: Result<serde_json::Value, String> = match op {
        "list" => list_desktops(),
        "switch" => parse_u32(&args, 2, "idx").and_then(switch_desktop),
        "create" => {
            let name = args.get(2).map(|s| s.as_str());
            create_desktop(name)
        }
        "move" => parse_isize(&args, 2, "hwnd")
            .and_then(|hwnd| parse_u32(&args, 3, "idx").map(|idx| (hwnd, idx)))
            .and_then(|(hwnd, idx)| move_window(hwnd, idx)),
        other => Err(format!("unknown operation: {}", other)),
    };

    match result {
        Ok(v) => {
            // Single-line JSON so the parent can serde_json::from_str the
            // entire stdout directly. Pretty-print would force the parent
            // to be more careful about reading the whole output first.
            println!("{}", v);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}", e);
            ExitCode::from(1)
        }
    }
}

// ---------------- arg helpers ---------------------------------------------

fn parse_u32(args: &[String], idx: usize, name: &str) -> Result<u32, String> {
    args.get(idx)
        .ok_or_else(|| format!("missing arg '{name}' at position {idx}"))
        .and_then(|s| {
            s.parse::<u32>()
                .map_err(|e| format!("invalid '{name}': {s}: {e}"))
        })
}

fn parse_isize(args: &[String], idx: usize, name: &str) -> Result<isize, String> {
    args.get(idx)
        .ok_or_else(|| format!("missing arg '{name}' at position {idx}"))
        .and_then(|s| {
            s.parse::<isize>()
                .map_err(|e| format!("invalid '{name}': {s}: {e}"))
        })
}

// ---------------- Windows implementations ---------------------------------

#[cfg(target_os = "windows")]
fn list_desktops() -> Result<serde_json::Value, String> {
    let desktops = winvd::get_desktops().map_err(|e| format!("get_desktops: {:?}", e))?;
    let current =
        winvd::get_current_desktop().map_err(|e| format!("get_current_desktop: {:?}", e))?;
    let current_idx = current
        .get_index()
        .map_err(|e| format!("get_index(current): {:?}", e))?;
    let mut names: Vec<String> = Vec::with_capacity(desktops.len());
    for d in &desktops {
        names.push(d.get_name().unwrap_or_default());
    }
    Ok(serde_json::json!({
        "count": desktops.len(),
        "current_idx": current_idx,
        "names": names,
    }))
}

#[cfg(target_os = "windows")]
fn switch_desktop(idx: u32) -> Result<serde_json::Value, String> {
    let d = winvd::get_desktop(idx);
    winvd::switch_desktop(d).map_err(|e| format!("switch_desktop: {:?}", e))?;
    Ok(serde_json::json!({ "ok": true, "idx": idx }))
}

#[cfg(target_os = "windows")]
fn create_desktop(name: Option<&str>) -> Result<serde_json::Value, String> {
    // Enforce Win11's 30-desktop cap here so the parent's logic doesn't
    // have to do a separate `list` call before `create`.
    let existing = winvd::get_desktops()
        .map_err(|e| format!("get_desktops: {:?}", e))?
        .len();
    if existing >= 30 {
        return Err(format!(
            "Windows 11 caps virtual desktops at 30; currently {existing}"
        ));
    }
    let d = winvd::create_desktop().map_err(|e| format!("create_desktop: {:?}", e))?;
    if let Some(n) = name {
        let _ = d.set_name(n);
    }
    let idx = d
        .get_index()
        .map_err(|e| format!("get_index(new): {:?}", e))?;
    Ok(serde_json::json!({
        "ok": true,
        "idx": idx,
        "name": name.unwrap_or(""),
    }))
}

#[cfg(target_os = "windows")]
fn move_window(hwnd_raw: isize, idx: u32) -> Result<serde_json::Value, String> {
    let d = winvd::get_desktop(idx);
    // windows 0.58's HWND wraps a *mut c_void instead of 0.52's isize.
    // Cast from the integer hwnd the parent passed us on the command
    // line. winvd 0.0.49 expects this exact type via its own windows
    // 0.58 dep.
    let hwnd = windows_58::Win32::Foundation::HWND(hwnd_raw as *mut std::ffi::c_void);
    winvd::move_window_to_desktop(d, &hwnd)
        .map_err(|e| format!("move_window_to_desktop: {:?}", e))?;
    Ok(serde_json::json!({ "ok": true, "hwnd": hwnd_raw, "idx": idx }))
}

// ---------------- non-Windows stubs ---------------------------------------

#[cfg(not(target_os = "windows"))]
fn list_desktops() -> Result<serde_json::Value, String> {
    Err("winvd_helper only works on Windows".to_string())
}
#[cfg(not(target_os = "windows"))]
fn switch_desktop(_idx: u32) -> Result<serde_json::Value, String> {
    Err("winvd_helper only works on Windows".to_string())
}
#[cfg(not(target_os = "windows"))]
fn create_desktop(_name: Option<&str>) -> Result<serde_json::Value, String> {
    Err("winvd_helper only works on Windows".to_string())
}
#[cfg(not(target_os = "windows"))]
fn move_window(_hwnd: isize, _idx: u32) -> Result<serde_json::Value, String> {
    Err("winvd_helper only works on Windows".to_string())
}
