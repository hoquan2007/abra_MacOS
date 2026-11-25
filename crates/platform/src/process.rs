use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, WaitForSingleObject, INFINITE, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE,
};

#[derive(Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    Exited,
    Unknown,
}

pub fn wait_for_exit(pid: u32) -> WaitOutcome {
    unsafe {
        match OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            Ok(h) => {
                let r = WaitForSingleObject(h, INFINITE);
                let _ = CloseHandle(h);
                if r == WAIT_OBJECT_0 {
                    WaitOutcome::Exited
                } else {
                    WaitOutcome::Unknown
                }
            }
            Err(_) if GetLastError() == ERROR_INVALID_PARAMETER => WaitOutcome::Exited,
            Err(_) => WaitOutcome::Unknown,
        }
    }
}

pub fn is_alive(pid: u32) -> bool {
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => {
                let mut code = 0u32;
                let ok = GetExitCodeProcess(h, &mut code).is_ok();
                let _ = CloseHandle(h);
                ok && code == STILL_ACTIVE
            }
            Err(_) => GetLastError() != ERROR_INVALID_PARAMETER,
        }
    }
}

pub fn image_name(pid: u32) -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::System::Threading::{QueryFullProcessImageNameW, PROCESS_NAME_WIN32};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        std::path::Path::new(&path).file_name().map(|n| n.to_string_lossy().to_lowercase())
    }
}

pub fn is_same_program(pid: u32) -> bool {
    let Some(mine) = std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase())) else { return false };
    is_alive(pid) && image_name(pid).is_some_and(|n| n == mine)
}

const PROTECTED_EXES: &[&str] = &[
    "explorer.exe",
    "applicationframehost.exe",
    "dwm.exe",
    "csrss.exe",
    "winlogon.exe",
    "wininit.exe",
    "lsass.exe",
    "services.exe",
    "smss.exe",
    "svchost.exe",
    "shellexperiencehost.exe",
    "startmenuexperiencehost.exe",
    "searchhost.exe",
    "textinputhost.exe",
    "sihost.exe",
    "ctfmon.exe",
];

pub fn is_protected_exe(file: &str) -> bool {
    let f = file.rsplit(['\\', '/']).next().unwrap_or("").to_ascii_lowercase();
    f.is_empty() || PROTECTED_EXES.contains(&f.as_str())
}

fn pid_of_window(hwnd: isize) -> u32 {
    let mut pid = 0u32;
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(windows::Win32::Foundation::HWND(hwnd as *mut _), Some(&mut pid));
    }
    pid
}

fn uwp_child_pid(frame: isize, frame_pid: u32) -> Option<u32> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::EnumChildWindows;
    struct Find {
        host: u32,
        found: Option<u32>,
    }
    unsafe extern "system" fn each(child: HWND, lp: LPARAM) -> BOOL {
        let f = &mut *(lp.0 as *mut Find);
        let pid = pid_of_window(child.0 as isize);
        if pid != 0 && pid != f.host {
            f.found = Some(pid);
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut f = Find { host: frame_pid, found: None };
    unsafe {
        let _ = EnumChildWindows(Some(HWND(frame as *mut _)), Some(each), LPARAM(&mut f as *mut Find as isize));
    }
    f.found
}

/// Tiến trình thật sở hữu cửa sổ; với app UWP thì là tiến trình con bên trong ApplicationFrameHost.
pub fn owner_pid(hwnd: isize) -> Option<u32> {
    let pid = pid_of_window(hwnd);
    if pid == 0 {
        return None;
    }
    if image_name(pid).as_deref() == Some("applicationframehost.exe") {
        return uwp_child_pid(hwnd, pid);
    }
    Some(pid)
}

/// PID có thể buộc thoát cho các cửa sổ này: bỏ trùng, bỏ tiến trình hệ thống và chính ABRA.
pub fn force_quit_targets(hwnds: &[isize]) -> Vec<u32> {
    let own = std::process::id();
    let mut out: Vec<u32> = Vec::new();
    for &h in hwnds {
        let Some(pid) = owner_pid(h) else { continue };
        if pid == own || out.contains(&pid) || is_same_program(pid) {
            continue;
        }
        match image_name(pid) {
            Some(n) if !is_protected_exe(&n) => out.push(pid),
            _ => {}
        }
    }
    out
}

/// Thử mở với quyền PROCESS_TERMINATE; thất bại khi app chạy quyền Admin còn ABRA thì không.
pub fn can_terminate(pid: u32) -> bool {
    use windows::Win32::System::Threading::PROCESS_TERMINATE;
    unsafe {
        match OpenProcess(PROCESS_TERMINATE, false, pid) {
            Ok(h) => {
                let _ = CloseHandle(h);
                true
            }
            Err(_) => false,
        }
    }
}

pub fn terminate(pid: u32) -> windows::core::Result<()> {
    use windows::Win32::System::Threading::{TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, false, pid)?;
        let r = TerminateProcess(h, 1);
        let _ = CloseHandle(h);
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_processes_are_protected() {
        assert!(is_protected_exe("explorer.exe"));
        assert!(is_protected_exe(r"C:\Windows\Explorer.EXE"));
        assert!(is_protected_exe("ApplicationFrameHost.exe"));
        assert!(is_protected_exe(""));
        assert!(!is_protected_exe("chrome.exe"));
        assert!(!is_protected_exe(r"C:\Program Files\Zalo\Zalo.exe"));
    }

    #[test]
    fn no_windows_means_no_targets() {
        assert!(force_quit_targets(&[]).is_empty());
        assert!(!is_protected_exe("notexplorer.exe"));
    }
}
