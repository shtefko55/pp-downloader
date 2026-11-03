//! Прозорците се отварят така, както са били затворени предния път (размер, позиция, цял екран).
//!
//! Прозорецът се създава скрит (`with_visible(false)`), в него се рисува първият кадър и после се показва
//! със `SetWindowPlacement` — размер, позиция и състояние наведнъж, без да се вижда междинен прозорец.
//! Това е механизмът на Windows за запомняне на прозорци; ако запомненото място вече е извън екраните
//! (махнат монитор), Windows сам премества прозореца на видимо място.
//! Първия път (няма запомнено) прозорецът се отваря на цял екран.

use eframe::egui;
use serde::{Deserialize, Serialize};

/// Запомненото състояние на прозорец: нормалният правоъгълник (координати на Windows) и дали е бил на цял екран.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct Place {
    pub maximized: bool,
    /// left, top, right, bottom
    pub rect: [i32; 4],
}

#[cfg(windows)]
mod imp {
    use super::Place;
    use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromRect, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GetSystemMetrics, GetWindowPlacement, SetWindowPlacement, SetWindowPos, ShowWindow, SM_CXPADDEDBORDER,
        SM_CXSIZEFRAME, SM_CYSIZEFRAME, SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED,
        SW_SHOWNORMAL, WINDOWPLACEMENT, WPF_RESTORETOMAXIMIZED,
    };

    fn placement(p: &Place, cmd: i32) -> WINDOWPLACEMENT {
        let [left, top, right, bottom] = p.rect;
        WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            flags: 0,
            showCmd: cmd as _,
            ptMinPosition: POINT { x: -1, y: -1 },
            ptMaxPosition: POINT { x: -1, y: -1 },
            rcNormalPosition: RECT { left, top, right, bottom },
        }
    }

    /// Докато прозорецът е още скрит: дава му крайния размер, за да се покаже направо такъв, когато
    /// eframe го направи видим след първия кадър. За цял екран — работната площ на монитора плюс
    /// невидимата рамка, точно както Windows разпъва прозорец.
    pub fn prepare(hwnd: isize, place: Option<Place>) {
        if hwnd == 0 {
            return;
        }
        let hwnd = hwnd as HWND;
        let r = match place {
            Some(p) => {
                // Нормалният размер (и при възстановяване от цял екран). SW_HIDE — прозорецът остава скрит.
                unsafe { SetWindowPlacement(hwnd, &placement(&p, SW_HIDE as i32)) };
                if !p.maximized {
                    return;
                }
                let [left, top, right, bottom] = p.rect;
                unsafe { MonitorFromRect(&RECT { left, top, right, bottom }, MONITOR_DEFAULTTONEAREST) }
            }
            None => unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) },
        };
        let mut mi: MONITORINFO = unsafe { std::mem::zeroed() };
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if unsafe { GetMonitorInfoW(r, &mut mi) } == 0 {
            return;
        }
        let w = mi.rcWork;
        let (bx, by) = unsafe {
            let pad = GetSystemMetrics(SM_CXPADDEDBORDER);
            (GetSystemMetrics(SM_CXSIZEFRAME) + pad, GetSystemMetrics(SM_CYSIZEFRAME) + pad)
        };
        unsafe {
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                w.left - bx,
                w.top - by,
                w.right - w.left + 2 * bx,
                w.bottom - w.top + 2 * by,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
    }

    pub fn show(hwnd: isize, place: Option<Place>) -> bool {
        if hwnd == 0 {
            return false;
        }
        let hwnd = hwnd as HWND;
        match place {
            Some(p) => {
                let cmd = if p.maximized { SW_SHOWMAXIMIZED } else { SW_SHOWNORMAL };
                unsafe { SetWindowPlacement(hwnd, &placement(&p, cmd as i32)) };
            }
            None => unsafe {
                ShowWindow(hwnd, SW_SHOWMAXIMIZED);
            },
        }
        true
    }

    pub fn get(hwnd: isize) -> Option<Place> {
        if hwnd == 0 {
            return None;
        }
        let mut wp: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
        wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        if unsafe { GetWindowPlacement(hwnd as HWND, &mut wp) } == 0 {
            return None;
        }
        let cmd = wp.showCmd as i32;
        let maximized = cmd == SW_SHOWMAXIMIZED as i32 || (cmd == SW_SHOWMINIMIZED as i32 && wp.flags & WPF_RESTORETOMAXIMIZED != 0);
        let r = wp.rcNormalPosition;
        (r.right > r.left && r.bottom > r.top).then_some(Place { maximized, rect: [r.left, r.top, r.right, r.bottom] })
    }

    pub fn find_by_title(title: &str) -> isize {
        let wide: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        unsafe { FindWindowW(std::ptr::null(), wide.as_ptr()) as isize }
    }

    pub fn frame_hwnd(frame: &eframe::Frame) -> isize {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        match frame.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => 0,
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Place;
    pub fn show(_: isize, _: Option<Place>) -> bool {
        false
    }
    pub fn prepare(_: isize, _: Option<Place>) {}
    pub fn get(_: isize) -> Option<Place> {
        None
    }
    pub fn find_by_title(_: &str) -> isize {
        0
    }
    pub fn frame_hwnd(_: &eframe::Frame) -> isize {
        0
    }
}

pub use imp::{find_by_title, frame_hwnd, get, prepare};

/// Показва скрития прозорец в запомненото състояние (или на цял екран, ако няма запомнено).
/// Ако прозорецът не бъде намерен, го показва по обичайния начин, за да не остане скрит.
/// `ctx` е контекстът на самия прозорец.
pub fn show(ctx: &egui::Context, hwnd: isize, place: Option<Place>) {
    if !imp::show(hwnd, place) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        if place.map_or(true, |p| p.maximized) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
        }
    }
}
