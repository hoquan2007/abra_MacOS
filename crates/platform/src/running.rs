use std::collections::{HashMap, HashSet};
use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId};

use crate::process;
use crate::wins::{enumerate, names};

/// Một app đang chạy, gom mọi tiến trình cùng file exe lại với nhau.
#[derive(Clone, Debug)]
pub struct RunningApp {
    pub path: String,
    pub name: String,
    pub pids: Vec<u32>,
    pub hwnds: Vec<isize>,
    /// Tổng bộ nhớ đang dùng (working set) của mọi tiến trình, tính bằng byte.
    pub mem: u64,
    /// true: có cửa sổ đang hiện; false: chạy nền (thường là app ở khay hệ thống).
    pub windowed: bool,
}

impl RunningApp {
    /// PID được phép buộc thoát: bỏ tiến trình hệ thống và chính ABRA.
    pub fn kill_targets(&self) -> Vec<u32> {
        let own = std::process::id();
        self.pids
            .iter()
            .copied()
            .filter(|&p| p != own && !process::is_same_program(p))
            .filter(|&p| process::image_name(p).is_some_and(|n| !process::is_protected_exe(&n)))
            .collect()
    }
}

fn session_of(pid: u32) -> Option<u32> {
    let mut sid = 0u32;
    unsafe { ProcessIdToSessionId(pid, &mut sid).ok().map(|_| sid) }
}

fn mem_of(pid: u32) -> u64 {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return 0 };
        let mut c = PROCESS_MEMORY_COUNTERS { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
        let ok = K32GetProcessMemoryInfo(h, &mut c, c.cb).as_bool();
        let _ = CloseHandle(h);
        if ok {
            c.WorkingSetSize as u64
        } else {
            0
        }
    }
}

fn all_pids() -> Vec<u32> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                if e.th32ProcessID != 0 {
                    out.push(e.th32ProcessID);
                }
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Mọi PID có ít nhất một cửa sổ cấp cao nhất, kể cả cửa sổ ẩn (app ở khay luôn có một cửa sổ ẩn như vậy).
fn pids_with_windows() -> HashSet<u32> {
    unsafe extern "system" fn each(hwnd: HWND, lp: LPARAM) -> BOOL {
        let set = &mut *(lp.0 as *mut HashSet<u32>);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != 0 {
            set.insert(pid);
        }
        BOOL(1)
    }
    let mut set = HashSet::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut set as *mut HashSet<u32> as isize));
    }
    set
}

fn file_stem(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    file.strip_suffix(".exe").or_else(|| file.strip_suffix(".EXE")).unwrap_or(file).to_string()
}

fn display_name(path: &str) -> String {
    let n = names::friendly(path);
    if n.is_empty() {
        file_stem(path)
    } else {
        n
    }
}

fn merge(into: &mut Vec<u32>, pids: &[u32]) {
    for &p in pids {
        if !into.contains(&p) {
            into.push(p);
        }
    }
}

/// Tiến trình phụ (ZaloCall.exe trong thư mục của Zalo.exe…) được gộp vào app chính thay vì hiện thành dòng riêng.
pub fn is_helper_of(helper: &str, main: &str) -> bool {
    let (h, m) = (helper.to_ascii_lowercase(), main.to_ascii_lowercase());
    if h == m || m.starts_with("shell:") {
        return false;
    }
    let Some((dir, _)) = m.rsplit_once('\\') else { return false };
    dir.matches('\\').count() >= 2 && h.starts_with(&format!("{dir}\\"))
}

/// App chạy nền có đáng hiện không: bỏ mọi thứ nằm trong thư mục Windows, tiến trình hệ thống và tiến trình "chủ" chung.
pub fn is_background_candidate(path: &str, windir: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let windir = windir.trim_end_matches('\\').to_ascii_lowercase();
    if lower.is_empty() || lower.starts_with(&format!("{windir}\\")) {
        return false;
    }
    !process::is_protected_exe(&lower) && !names::is_host(&lower)
}

/// Ảnh chụp các app đang chạy của người dùng hiện tại: app có cửa sổ trước, rồi app chạy nền, mỗi nhóm xếp theo tên.
pub fn snapshot() -> Vec<RunningApp> {
    let own = std::process::id();
    let my_session = session_of(own);
    let own_exe = std::env::current_exe().map(|p| p.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());

    let mut by_path: HashMap<String, (String, Vec<u32>)> = HashMap::new();
    for pid in all_pids() {
        if pid == own || session_of(pid) != my_session {
            continue;
        }
        let Some(path) = enumerate::exe_path_of_pid(pid) else { continue };
        by_path.entry(path.to_ascii_lowercase()).or_insert_with(|| (path, Vec::new())).1.push(pid);
    }

    let mut apps: Vec<RunningApp> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for w in enumerate::list(own) {
        let key = w.path.to_ascii_lowercase();
        let i = *index.entry(key.clone()).or_insert_with(|| {
            let name = if w.name.is_empty() { display_name(&w.path) } else { w.name.clone() };
            apps.push(RunningApp { path: w.path.clone(), name, pids: Vec::new(), hwnds: Vec::new(), mem: 0, windowed: true });
            apps.len() - 1
        });
        let app = &mut apps[i];
        app.hwnds.push(w.hwnd);
        if let Some(pid) = process::owner_pid(w.hwnd) {
            if !app.pids.contains(&pid) {
                app.pids.push(pid);
            }
        }
        if let Some((_, pids)) = by_path.get(&key) {
            for &p in pids {
                if !app.pids.contains(&p) {
                    app.pids.push(p);
                }
            }
        }
    }

    let with_windows = pids_with_windows();
    let mut background: Vec<RunningApp> = Vec::new();
    let mut rest: Vec<(String, (String, Vec<u32>))> = by_path
        .into_iter()
        .filter(|(key, _)| !index.contains_key(key) && *key != own_exe && is_background_candidate(key, &windir))
        .collect();
    rest.sort_by_key(|(key, _)| key.len());
    for (key, (path, pids)) in rest {
        if let Some(i) = apps.iter().position(|a| is_helper_of(&key, &a.path)) {
            merge(&mut apps[i].pids, &pids);
        } else if let Some(b) = background.iter_mut().find(|b| is_helper_of(&key, &b.path)) {
            merge(&mut b.pids, &pids);
        } else if pids.iter().any(|p| with_windows.contains(p)) {
            background.push(RunningApp { name: display_name(&path), path, pids, hwnds: Vec::new(), mem: 0, windowed: false });
        }
    }

    let by_name = |a: &RunningApp, b: &RunningApp| a.name.to_lowercase().cmp(&b.name.to_lowercase());
    apps.sort_by(by_name);
    background.sort_by(by_name);
    apps.extend(background);
    for app in &mut apps {
        app.mem = app.pids.iter().map(|&p| mem_of(p)).sum();
    }
    apps
}

/// "245 MB", "1,2 GB"… dạng ngắn để đặt ở cột phải.
pub fn format_mem(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let mb = bytes as f64 / MB;
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else if mb >= 1.0 {
        format!("{} MB", mb.round() as u64)
    } else if bytes > 0 {
        "<1 MB".to_string()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_folder_and_system_processes_are_not_background_apps() {
        let w = r"C:\Windows";
        assert!(!is_background_candidate(r"C:\Windows\System32\svchost.exe", w));
        assert!(!is_background_candidate(r"c:\windows\explorer.exe", w));
        assert!(!is_background_candidate(r"C:\Program Files\Foo\dllhost.exe", w));
        assert!(!is_background_candidate("", w));
        assert!(is_background_candidate(r"C:\Users\me\AppData\Local\Programs\Zalo\Zalo.exe", w));
        assert!(is_background_candidate(r"C:\Program Files\Cloudflare\Cloudflare WARP\Cloudflare WARP.exe", w));
        assert!(is_background_candidate(r"C:\WindowsApps\x.exe", w), "chỉ đúng thư mục Windows mới bị loại");
    }

    #[test]
    fn helpers_inside_the_install_folder_join_the_main_app() {
        let zalo = r"C:\Users\me\AppData\Local\Programs\Zalo\Zalo-26\Zalo.exe";
        assert!(is_helper_of(r"C:\Users\me\AppData\Local\Programs\Zalo\Zalo-26\plugins\capture\ZaloCall.exe", zalo));
        assert!(!is_helper_of(zalo, zalo));
        assert!(!is_helper_of(r"C:\Users\me\AppData\Local\Programs\Other\Other.exe", zalo));
        assert!(!is_helper_of(r"C:\Tools\x.exe", r"C:\Tools.exe"));
        assert!(!is_helper_of(r"C:\Apps\b.exe", r"C:\Apps\a.exe"), "thư mục quá nông thì không gộp");
        assert!(!is_helper_of(r"C:\Program Files\X\x.exe", r"shell:appsfolder\Foo!App"));
    }

    #[test]
    fn memory_is_formatted_compactly() {
        assert_eq!(format_mem(0), "");
        assert_eq!(format_mem(1000), "<1 MB");
        assert_eq!(format_mem(245 * 1024 * 1024), "245 MB");
        assert_eq!(format_mem(1288 * 1024 * 1024), "1.3 GB");
    }

    #[test]
    fn name_falls_back_to_file_stem() {
        assert_eq!(file_stem(r"C:\Apps\Zalo.exe"), "Zalo");
        assert_eq!(file_stem("tool"), "tool");
    }

    #[test]
    #[ignore = "in danh sách thật để xem bằng mắt: cargo test -p nacre-platform print_snapshot -- --ignored --nocapture"]
    fn print_snapshot() {
        for a in snapshot() {
            println!("{} {:<32} {:>9} pids={:?} {}", if a.windowed { "[win]" } else { "[bg] " }, a.name, format_mem(a.mem), a.pids, a.path);
        }
    }

    #[test]
    fn snapshot_never_lists_abra_itself() {
        let own = std::process::id();
        assert!(snapshot().iter().all(|a| !a.pids.contains(&own)));
    }
}
