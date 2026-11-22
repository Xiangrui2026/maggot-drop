use std::{
    cell::RefCell,
    collections::VecDeque,
    ffi::{OsStr, c_void},
    io,
    mem::size_of,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::{copy_nonoverlapping, null, null_mut},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use windows_sys::Win32::{
    Foundation::{GlobalFree, HGLOBAL, HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::{
        DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW,
            SetClipboardData,
        },
        LibraryLoader::GetModuleHandleW,
        Memory::{GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock},
        Ole::{CF_HDROP, DROPEFFECT_COPY},
    },
    UI::{
        Shell::{
            DROPFILES, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIM_ADD,
            NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_BALLOONHIDE, NIN_BALLOONTIMEOUT,
            NIN_BALLOONUSERCLICK, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
            ShellExecuteW,
        },
        WindowsAndMessaging::*,
    },
};

use crate::network::{self, ClientConfig, Downloaded};

const TRAY_EVENT: u32 = WM_APP + 1;
const TIMER_ID: usize = 1;
const COPY_LAST: u32 = 1;
const OPEN_DOWNLOADS: u32 = 2;
const QUIT: u32 = 3;

thread_local! {
    static STATE: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

struct Tray {
    notifications: mpsc::UnboundedReceiver<Downloaded>,
    download_dir: PathBuf,
    pending: VecDeque<Downloaded>,
    active: Option<(Downloaded, Instant)>,
    last: Option<PathBuf>,
    taskbar_created: u32,
    registered: bool,
}

pub fn run() -> Result<()> {
    let config = ClientConfig::load()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (sender, notifications) = mpsc::unbounded_channel();
    let class = wide("MaggotDrop.Tray");
    let title = wide("MaggotDrop");
    let instance = unsafe { GetModuleHandleW(null()) };
    let window_class = WNDCLASSW {
        hInstance: instance,
        lpfnWndProc: Some(window_proc),
        lpszClassName: class.as_ptr(),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&window_class) } == 0 {
        return Err(io::Error::last_os_error()).context("无法注册客户端窗口");
    }
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        )
    };
    if hwnd.is_null() {
        return Err(io::Error::last_os_error()).context("无法创建客户端窗口");
    }

    let download_dir = config.download_dir.clone();
    STATE.with(|state| {
        *state.borrow_mut() = Some(Tray {
            notifications,
            download_dir: download_dir.clone(),
            pending: VecDeque::new(),
            active: None,
            last: None,
            taskbar_created: unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) },
            registered: register_icon(hwnd),
        });
    });
    if unsafe { SetTimer(hwnd, TIMER_ID, 350, None) } == 0 {
        return Err(io::Error::last_os_error()).context("无法启动通知定时器");
    }
    thread::Builder::new()
        .name("MaggotDrop network".into())
        .spawn(move || {
            if let Err(error) = runtime.block_on(network::run(config, sender)) {
                network::log(&download_dir, &format!("客户端连接服务停止: {error:#}"));
            }
        })?;

    let mut message = MSG::default();
    loop {
        match unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } {
            -1 => return Err(io::Error::last_os_error()).context("读取客户端消息失败"),
            0 => break,
            _ => unsafe { DispatchMessageW(&message) },
        };
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_DESTROY {
        unsafe {
            KillTimer(hwnd, TIMER_ID);
            Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
            PostQuitMessage(0);
        }
        return 0;
    }
    if message == TRAY_EVENT {
        match lparam as u32 & 0xffff {
            NIN_BALLOONUSERCLICK => {
                if let Some((file, _)) = with_tray(|tray| tray.active.take()).flatten() {
                    copy_or_report(hwnd, &file.path);
                }
            }
            NIN_BALLOONHIDE | NIN_BALLOONTIMEOUT => {
                with_tray(|tray| {
                    tray.active = None;
                });
            }
            WM_CONTEXTMENU | WM_RBUTTONUP => tray_menu(hwnd),
            WM_LBUTTONDBLCLK => copy_last(hwnd),
            _ => {}
        }
        return 0;
    }
    if message == WM_TIMER && wparam == TIMER_ID {
        with_tray(|tray| tray.show_next(hwnd));
        return 0;
    }
    if with_tray(|tray| {
        if message == 0 || message != tray.taskbar_created {
            return false;
        }
        tray.registered = false;
        if let Some((file, _)) = tray.active.take() {
            tray.pending.push_front(file);
        }
        true
    }) == Some(true)
    {
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn with_tray<T>(action: impl FnOnce(&mut Tray) -> T) -> Option<T> {
    STATE.with(|state| Some(action(state.try_borrow_mut().ok()?.as_mut()?)))
}

impl Tray {
    fn show_next(&mut self, hwnd: HWND) {
        for _ in 0..128 {
            let Ok(file) = self.notifications.try_recv() else {
                break;
            };
            self.last = Some(file.path.clone());
            self.pending.push_back(file);
        }
        if let Some((_, requested)) = &self.active {
            if requested.elapsed() >= Duration::from_secs(60) {
                // A disabled or unresponsive shell may omit the dismissal callback.
                let mut icon = icon_data(hwnd);
                icon.uFlags = NIF_INFO;
                self.registered = unsafe { Shell_NotifyIconW(NIM_MODIFY, &icon) != 0 };
                self.active = None;
            }
            return;
        }
        if !self.registered {
            self.registered = register_icon(hwnd);
            if !self.registered {
                return;
            }
        }
        if let Some(file) = self.pending.pop_front() {
            self.registered = show_balloon(hwnd, &file.name);
            if self.registered {
                self.active = Some((file, Instant::now()));
            } else {
                self.pending.push_front(file);
            }
        }
    }
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    }
}

fn register_icon(hwnd: HWND) -> bool {
    let mut icon = icon_data(hwnd);
    icon.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    icon.uCallbackMessage = TRAY_EVENT;
    icon.hIcon = unsafe { LoadIconW(null_mut(), IDI_APPLICATION) };
    fill_wide(&mut icon.szTip, "MaggotDrop");
    if unsafe {
        Shell_NotifyIconW(NIM_ADD, &icon) == 0 && Shell_NotifyIconW(NIM_MODIFY, &icon) == 0
    } {
        return false;
    }
    icon.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    unsafe { Shell_NotifyIconW(NIM_SETVERSION, &icon) };
    true
}

fn show_balloon(hwnd: HWND, filename: &str) -> bool {
    let mut icon = icon_data(hwnd);
    icon.uFlags = NIF_INFO;
    icon.dwInfoFlags = NIIF_INFO;
    fill_wide(&mut icon.szInfoTitle, "MaggotDrop · 文件已接收");
    fill_wide(&mut icon.szInfo, &format!("点击复制文件\n{filename}"));
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &icon) != 0 }
}

fn tray_menu(hwnd: HWND) {
    let has_file = with_tray(|tray| tray.last.is_some()).unwrap_or(false);
    let mut point = POINT::default();
    unsafe {
        GetCursorPos(&mut point);
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let copy_flags = if has_file {
            MF_STRING
        } else {
            MF_STRING | MF_GRAYED
        };
        AppendMenuW(
            menu,
            copy_flags,
            COPY_LAST as usize,
            wide("复制最近接收的文件").as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING,
            OPEN_DOWNLOADS as usize,
            wide("打开下载目录").as_ptr(),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING, QUIT as usize, wide("退出").as_ptr());
        SetForegroundWindow(hwnd);
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            hwnd,
            null(),
        ) as u32;
        DestroyMenu(menu);
        PostMessageW(hwnd, WM_NULL, 0, 0);
        match command {
            COPY_LAST => copy_last(hwnd),
            OPEN_DOWNLOADS => {
                if let Some(directory) = with_tray(|tray| wide(&tray.download_dir)) {
                    ShellExecuteW(
                        hwnd,
                        wide("open").as_ptr(),
                        directory.as_ptr(),
                        null(),
                        null(),
                        SW_SHOWNORMAL,
                    );
                }
            }
            QUIT => {
                DestroyWindow(hwnd);
            }
            _ => {}
        }
    }
}

fn copy_last(hwnd: HWND) {
    if let Some(path) = with_tray(|tray| tray.last.clone()).flatten() {
        copy_or_report(hwnd, &path);
    }
}

fn copy_or_report(hwnd: HWND, path: &Path) {
    if let Err(error) = copy_file(hwnd, path) {
        report_error(&format!("无法复制文件。\n{error:#}"));
    }
}

fn copy_file(hwnd: HWND, path: &Path) -> Result<()> {
    anyhow::ensure!(path.is_file(), "下载的文件已被移动或删除");
    let mut filenames = wide(path);
    filenames.push(0);
    let header = DROPFILES {
        pFiles: size_of::<DROPFILES>() as u32,
        fWide: 1,
        ..Default::default()
    };
    let drop_files = GlobalBlock::new(
        size_of::<DROPFILES>() + filenames.len() * size_of::<u16>(),
        |ptr| {
            // CF_HDROP stores a DROPFILES followed by a double-NUL-terminated UTF-16 path list.
            unsafe {
                ptr.cast::<DROPFILES>().write(header);
                copy_nonoverlapping(
                    filenames.as_ptr(),
                    ptr.cast::<u8>().add(size_of::<DROPFILES>()).cast(),
                    filenames.len(),
                );
            }
        },
    )?;
    let copy_effect = GlobalBlock::new(size_of::<u32>(), |ptr| unsafe {
        ptr.cast::<u32>().write(DROPEFFECT_COPY);
    })?;
    let preferred = unsafe { RegisterClipboardFormatW(wide("Preferred DropEffect").as_ptr()) };

    let _clipboard = Clipboard::open(hwnd)?;
    if unsafe { EmptyClipboard() } == 0 {
        return Err(io::Error::last_os_error()).context("无法清空剪贴板");
    }
    drop_files.set(u32::from(CF_HDROP))?;
    if preferred != 0 {
        let _ = copy_effect.set(preferred);
    }
    Ok(())
}

struct Clipboard;

impl Clipboard {
    fn open(hwnd: HWND) -> Result<Self> {
        for attempt in 0..6 {
            if unsafe { OpenClipboard(hwnd) } != 0 {
                return Ok(Self);
            }
            if attempt < 5 {
                thread::sleep(Duration::from_millis(20));
            }
        }
        Err(io::Error::last_os_error()).context("无法打开剪贴板")
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        unsafe { CloseClipboard() };
    }
}

struct GlobalBlock(HGLOBAL);

impl GlobalBlock {
    fn new(size: usize, fill: impl FnOnce(*mut c_void)) -> Result<Self> {
        let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, size) };
        if memory.is_null() {
            return Err(io::Error::last_os_error()).context("无法分配剪贴板内存");
        }
        let block = Self(memory);
        let storage = unsafe { GlobalLock(block.0) };
        if storage.is_null() {
            return Err(io::Error::last_os_error()).context("无法访问剪贴板内存");
        }
        fill(storage);
        unsafe { GlobalUnlock(block.0) };
        Ok(block)
    }

    fn set(mut self, format: u32) -> Result<()> {
        if unsafe { SetClipboardData(format, self.0) }.is_null() {
            return Err(io::Error::last_os_error()).context("无法写入剪贴板");
        }
        // Windows owns the handle after SetClipboardData succeeds.
        self.0 = null_mut();
        Ok(())
    }
}

impl Drop for GlobalBlock {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { GlobalFree(self.0) };
        }
    }
}

pub fn report_error(message: &str) {
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(message).as_ptr(),
            wide("MaggotDrop").as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain([0]).collect()
}

fn fill_wide(target: &mut [u16], text: &str) {
    let mut index = 0;
    for ch in text.chars() {
        let end = index + ch.len_utf16();
        if end >= target.len() {
            break;
        }
        ch.encode_utf16(&mut target[index..end]);
        index = end;
    }
}
