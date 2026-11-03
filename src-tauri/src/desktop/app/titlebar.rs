//! Window-chrome corrections that the Tauri builder does not cover.

/// A maximized undecorated window on Windows covers the taskbar: without
/// `WS_CAPTION` the system sizes the maximized rect to the whole monitor
/// (1920x1080 on this machine) instead of the work area (1920x1015), so the
/// bottom 65px sit on top of the taskbar. This runs on every resize while
/// the window is zoomed and pins the rect to the monitor's work area. The
/// window stays maximized (`IsZoomed` is untouched) and restore keeps the
/// pre-maximize rect; every maximize path -- our button, Win+Up,
/// drag-to-edge snap -- flows through here because the correction is
/// event-driven, not button-driven.
#[cfg(windows)]
pub fn keep_maximized_in_work_area(window: &tauri::Window) {
  use windows_sys::Win32::Foundation::{HWND, RECT};
  use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
  };
  use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, IsZoomed, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER,
  };

  let Ok(handle) = window.hwnd() else { return };
  let hwnd = handle.0 as HWND;
  unsafe {
    if IsZoomed(hwnd) == 0 {
      return;
    }
    let mut rect = std::mem::zeroed::<RECT>();
    if GetWindowRect(hwnd, &mut rect) == 0 {
      return;
    }
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let mut info = std::mem::zeroed::<MONITORINFO>();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(monitor, &mut info) == 0 {
      return;
    }
    let work = info.rcWork;
    if rect.left == work.left
      && rect.top == work.top
      && rect.right == work.right
      && rect.bottom == work.bottom
    {
      return;
    }
    SetWindowPos(
      hwnd,
      std::ptr::null_mut(),
      work.left,
      work.top,
      work.right - work.left,
      work.bottom - work.top,
      SWP_NOZORDER | SWP_NOACTIVATE,
    );
  }
}

#[cfg(not(windows))]
pub fn keep_maximized_in_work_area(_window: &tauri::Window) {}
