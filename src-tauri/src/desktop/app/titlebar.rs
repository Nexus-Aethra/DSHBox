//! Window-chrome corrections that the Tauri builder does not cover.

/// A maximized undecorated window on Windows covers the taskbar: without
/// `WS_CAPTION` the system sizes the maximized rect to the whole monitor
/// (1920x1080 on this machine) instead of the work area (1920x1015), so the
/// bottom ~56px sit on top of the taskbar. This runs on every resize while
/// the window is zoomed and pins the CLIENT area to the monitor's work
/// area. The window stays maximized (`IsZoomed` is untouched) and restore
/// keeps the pre-maximize rect; every maximize path -- our button, Win+Up,
/// drag-to-edge snap -- flows through here because the correction is
/// event-driven, not button-driven.
///
/// The comparison is against the client area, not the window rect: after
/// the first correction the system wraps the standard invisible resize
/// frame around the client, so the window rect never equals the work area
/// again (it sits at `-frame..+frame` around it forever). A window-rect
/// comparison would re-issue the move on every resize event; a client
/// comparison makes the second pass a true no-op, which is also what ends
/// the resize-event chain this handler itself causes.
#[cfg(windows)]
pub fn keep_maximized_in_work_area(window: &tauri::Window) {
  use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
  use windows_sys::Win32::Graphics::Gdi::{
    ClientToScreen, GetMonitorInfoW, MonitorFromWindow, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
  };
  use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClientRect, IsZoomed, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER,
  };

  let Ok(handle) = window.hwnd() else { return };
  let hwnd = handle.0 as HWND;
  unsafe {
    if IsZoomed(hwnd) == 0 {
      return;
    }
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let mut info = std::mem::zeroed::<MONITORINFO>();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(monitor, &mut info) == 0 {
      return;
    }
    let work = info.rcWork;
    let work_w = work.right - work.left;
    let work_h = work.bottom - work.top;
    let mut client = std::mem::zeroed::<RECT>();
    if GetClientRect(hwnd, &mut client) == 0 {
      return;
    }
    let mut origin = POINT { x: client.left, y: client.top };
    if ClientToScreen(hwnd, &mut origin) == 0 {
      return;
    }
    let client_w = client.right - client.left;
    let client_h = client.bottom - client.top;
    if origin.x == work.left
      && origin.y == work.top
      && client_w == work_w
      && client_h == work_h
    {
      return;
    }
    SetWindowPos(
      hwnd,
      std::ptr::null_mut(),
      work.left,
      work.top,
      work_w,
      work_h,
      SWP_NOZORDER | SWP_NOACTIVATE,
    );
  }
}

#[cfg(not(windows))]
pub fn keep_maximized_in_work_area(_window: &tauri::Window) {}
