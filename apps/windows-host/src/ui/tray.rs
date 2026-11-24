use gpui::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::{
        Shell::{
            DefSubclassProc, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
            NOTIFYICONDATAW, RemoveWindowSubclass, SetWindowSubclass, Shell_NotifyIconW,
        },
        WindowsAndMessaging::{
            IDI_APPLICATION, IsWindowVisible, LoadIconW, RegisterWindowMessageW, SC_MINIMIZE,
            SW_HIDE, SW_RESTORE, SetForegroundWindow, ShowWindow, WM_APP, WM_LBUTTONUP,
            WM_NCDESTROY, WM_SYSCOMMAND,
        },
    },
};

const ICON_ID: u32 = 1;
const TRAY_MESSAGE: u32 = WM_APP + 1;

fn hwnd(window: &Window) -> Option<HWND> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return None;
    };
    Some(handle.hwnd.get() as HWND)
}

pub(super) fn attach(window: &Window) -> bool {
    let Some(hwnd) = hwnd(window) else {
        return false;
    };
    let message_name: Vec<u16> = "TaskbarCreated\0".encode_utf16().collect();
    // The GPUI window and its subclass run on the same UI thread.
    unsafe {
        let taskbar_created = RegisterWindowMessageW(message_name.as_ptr());
        taskbar_created != 0
            && SetWindowSubclass(
                hwnd,
                Some(window_proc),
                ICON_ID as usize,
                taskbar_created as usize,
            ) != 0
            && add_icon(hwnd)
    }
}

pub(super) fn minimize(window: &Window) {
    use windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW;
    if let Some(hwnd) = hwnd(window) {
        unsafe { SendMessageW(hwnd, WM_SYSCOMMAND, SC_MINIMIZE as usize, 0) };
    }
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        ..Default::default()
    }
}

fn add_icon(hwnd: HWND) -> bool {
    let mut icon = icon_data(hwnd);
    icon.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    icon.uCallbackMessage = TRAY_MESSAGE;
    icon.hIcon = unsafe { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) };
    for (slot, unit) in icon.szTip.iter_mut().zip("Hyper Seamless X".encode_utf16()) {
        *slot = unit;
    }
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &icon) != 0 || Shell_NotifyIconW(NIM_ADD, &icon) != 0 }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass: usize,
    taskbar_created: usize,
) -> LRESULT {
    unsafe {
        if message == taskbar_created as u32 {
            if !add_icon(hwnd) && IsWindowVisible(hwnd) == 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
        } else if message == WM_SYSCOMMAND && wparam & 0xfff0 == SC_MINIMIZE as usize {
            // Keep standard minimization as a fallback when the shell is unavailable.
            if add_icon(hwnd) {
                ShowWindow(hwnd, SW_HIDE);
                return 0;
            }
        } else if message == TRAY_MESSAGE && wparam == ICON_ID as usize {
            if lparam as u32 == WM_LBUTTONUP {
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
            }
            return 0;
        } else if message == WM_NCDESTROY {
            Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
            RemoveWindowSubclass(hwnd, Some(window_proc), subclass);
        }
        DefSubclassProc(hwnd, message, wparam, lparam)
    }
}
