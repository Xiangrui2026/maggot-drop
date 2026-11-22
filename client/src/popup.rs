use std::{
    io,
    ptr::{null, null_mut},
};

use anyhow::{Context, Result};
use windows_sys::Win32::{
    Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{COLOR_WINDOW, DEFAULT_GUI_FONT, GetStockObject},
    System::SystemServices::{SS_ENDELLIPSIS, SS_NOPREFIX},
    UI::WindowsAndMessaging::*,
};

use crate::tray::wide;

pub const POPUP_EVENT: u32 = WM_APP + 2;
pub const COPY: WPARAM = 1;
pub const DISMISS: WPARAM = 2;
const WIDTH: i32 = 360;
const HEIGHT: i32 = 140;
const MARGIN: i32 = 16;

pub struct Popup {
    window: HWND,
    filename: HWND,
    feedback: HWND,
}

impl Popup {
    pub fn new(owner: HWND, instance: HINSTANCE) -> Result<Self> {
        let class = wide("MaggotDrop.DownloadPopup");
        let window_class = WNDCLASSW {
            hInstance: instance,
            lpfnWndProc: Some(window_proc),
            lpszClassName: class.as_ptr(),
            hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
            hbrBackground: (COLOR_WINDOW as usize + 1) as _,
            ..Default::default()
        };
        if unsafe { RegisterClassW(&window_class) } == 0 {
            return Err(io::Error::last_os_error()).context("无法注册文件弹窗");
        }
        let window = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class.as_ptr(),
                wide("MaggotDrop · 文件已接收").as_ptr(),
                WS_POPUP | WS_BORDER,
                0,
                0,
                WIDTH,
                HEIGHT,
                owner,
                null_mut(),
                instance,
                null(),
            )
        };
        if window.is_null() {
            return Err(io::Error::last_os_error()).context("无法创建文件弹窗");
        }

        let label = |text, top| {
            control(
                window,
                "STATIC",
                text,
                SS_NOPREFIX | SS_ENDELLIPSIS,
                0,
                [16, top, 326, 20],
            )
        };
        label("文件已下载", 14)?;
        let filename = label("", 38)?;
        let feedback = label("", 64)?;
        control(
            window,
            "BUTTON",
            "复制文件",
            BS_DEFPUSHBUTTON as u32,
            COPY,
            [16, 94, 108, 28],
        )?;
        control(
            window,
            "BUTTON",
            "关闭",
            BS_PUSHBUTTON as u32,
            DISMISS,
            [132, 94, 72, 28],
        )?;
        Ok(Self {
            window,
            filename,
            feedback,
        })
    }

    pub fn show(&self, filename: &str) -> Result<()> {
        let mut work = RECT::default();
        if unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut work as *mut RECT).cast(), 0) }
            == 0
        {
            return Err(io::Error::last_os_error()).context("无法读取屏幕工作区");
        }
        let x = (work.right - WIDTH - MARGIN).max(work.left);
        let y = (work.bottom - HEIGHT - MARGIN).max(work.top);
        unsafe { SetWindowTextW(self.filename, wide(filename).as_ptr()) };
        self.feedback("");
        if unsafe {
            SetWindowPos(
                self.window,
                HWND_TOPMOST,
                x,
                y,
                WIDTH,
                HEIGHT,
                SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
            )
        } == 0
        {
            return Err(io::Error::last_os_error()).context("无法显示文件弹窗");
        }
        Ok(())
    }

    pub fn hide(&self) {
        unsafe { ShowWindow(self.window, SW_HIDE) };
    }

    pub fn feedback(&self, text: &str) {
        unsafe { SetWindowTextW(self.feedback, wide(text).as_ptr()) };
    }
}

fn control(
    parent: HWND,
    class: &str,
    label: &str,
    style: u32,
    id: usize,
    [x, y, width, height]: [i32; 4],
) -> Result<HWND> {
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            wide(class).as_ptr(),
            wide(label).as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            x,
            y,
            width,
            height,
            parent,
            id as HMENU,
            null_mut(),
            null(),
        )
    };
    if hwnd.is_null() {
        return Err(io::Error::last_os_error()).context("无法创建文件弹窗控件");
    }
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETFONT,
            GetStockObject(DEFAULT_GUI_FONT) as WPARAM,
            0,
        )
    };
    Ok(hwnd)
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_COMMAND if lparam != 0 && (wparam >> 16) == BN_CLICKED as usize => {
            let action = wparam & 0xffff;
            if matches!(action, COPY | DISMISS) {
                unsafe { SendMessageW(GetWindow(hwnd, GW_OWNER), POPUP_EVENT, action, 0) };
            }
            0
        }
        WM_CLOSE => {
            unsafe { SendMessageW(GetWindow(hwnd, GW_OWNER), POPUP_EVENT, DISMISS, 0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
