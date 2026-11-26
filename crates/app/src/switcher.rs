//! Pengalih app bergaya macOS (Cmd+Tab): satu baris ikon besar di panel kaca,
//! sorotan kotak membulat di belakang ikon terpilih, nama app di bawahnya.
//! Tab / Shift+Tab / ` / ← → untuk berpindah, Q menutup app, H menyembunyikan,
//! Esc membatalkan, melepas Alt berpindah ke app terpilih.

use crate::popup::Popup;
use crate::{motion, prefs};
use nacre_platform::icons;
use nacre_platform::text::{measure, render, Style};
use nacre_platform::wins::{enumerate, focus, tracker};
use nacre_ui::gfx::Gfx;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, GetAsyncKeyState, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_ESCAPE, VK_LEFT, VK_LMENU, VK_OEM_3, VK_RETURN, VK_RIGHT,
    VK_RMENU, VK_SHIFT, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::UI::Color;
use windows::UI::Composition::{CompositionStretch, Compositor, ContainerVisual, SpriteVisual};
use windows_numerics::{Vector2, Vector3};

const WM_NEXT: u32 = WM_APP + 120;
const WM_PREV: u32 = WM_APP + 121;
const WM_COMMIT: u32 = WM_APP + 122;
const WM_CANCEL: u32 = WM_APP + 123;
const WM_INSTALL: u32 = WM_APP + 124;
const WM_QUIT_APP: u32 = WM_APP + 125;
const WM_HIDE_APP: u32 = WM_APP + 126;

/// Tombol virtual tak terpakai; dikirim saat Alt ditahan agar app di depan
/// tidak mengaktifkan bilah menunya ketika Alt dilepas.
const VK_DUMMY: u8 = 0xE8;
const VK_Q: u16 = 0x51;
const VK_H: u16 = 0x48;
const MAX_APPS: usize = 14;

// Ukuran logis (dikali DPI).
const ICON: f32 = 92.0;
const CELL: f32 = 116.0;
const GAP: f32 = 4.0;
const PAD_X: f32 = 14.0;
const PAD_TOP: f32 = 14.0;
const LABEL_H: f32 = 34.0;
const RADIUS: f32 = 24.0;
const SEL_RADIUS: f32 = 18.0;

static HOOK: std::sync::Mutex<Option<nacre_platform::hookthread::HookThread>> = std::sync::Mutex::new(None);
static OPEN: AtomicBool = AtomicBool::new(false);
static TARGET: AtomicIsize = AtomicIsize::new(0);

#[derive(Clone)]
struct Entry {
    path: String,
    name: String,
    hwnd: isize,
}

struct Ui {
    popup: Popup,
    entries: Vec<Entry>,
    sel: usize,
    cells: Vec<(f32, f32, f32, f32)>,
    highlight: SpriteVisual,
    label: ContainerVisual,
    u: f32,
    pw: f32,
    fg: [u8; 3],
}

struct Ctl {
    hwnd: HWND,
    comp: Compositor,
    gfx: Option<Gfx>,
    ui: Option<Ui>,
}

thread_local! {
    static CTL: RefCell<Option<Ctl>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Ctl) -> R) -> Option<R> {
    CTL.with(|c| c.try_borrow_mut().ok().and_then(|mut b| b.as_mut().map(f)))
}

pub fn init(comp: &Compositor) -> windows_core::Result<()> {
    unsafe {
        let hi: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = w!("AbraSwitcherCtl");
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(ctl_proc), hInstance: hi, lpszClassName: class, ..Default::default() });
        let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), class, w!("abra-switcher"), WS_POPUP, 0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(hi), None)?;
        TARGET.store(hwnd.0 as isize, Ordering::Relaxed);
        CTL.with(|c| *c.borrow_mut() = Some(Ctl { hwnd, comp: comp.clone(), gfx: None, ui: None }));
        prefs::listen(hwnd);
    }
    sync();
    Ok(())
}

pub fn shutdown() {
    uninstall();
}

fn sync() {
    // Pengalih kini berdiri sendiri, tidak lagi bergantung pada Scene (Stage Manager).
    if prefs::get().switcher {
        install();
    } else {
        uninstall();
        with(|c| c.close());
    }
}

fn install() {
    let mut g = HOOK.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        *g = nacre_platform::hookthread::HookThread::start(nacre_platform::hookthread::Kind::Keyboard, Some(ll_proc));
    }
}

fn uninstall() {
    let h = HOOK.lock().unwrap_or_else(|e| e.into_inner()).take();
    drop(h);
    OPEN.store(false, Ordering::Relaxed);
}

unsafe extern "system" fn ll_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        let down = wp.0 as u32 == WM_KEYDOWN || wp.0 as u32 == WM_SYSKEYDOWN;
        let up = wp.0 as u32 == WM_KEYUP || wp.0 as u32 == WM_SYSKEYUP;
        let alt = k.flags.0 & LLKHF_ALTDOWN.0 != 0;
        let open = OPEN.load(Ordering::Relaxed);
        let target = HWND(TARGET.load(Ordering::Relaxed) as *mut _);
        let injected = k.flags.0 & LLKHF_INJECTED.0 != 0 && k.vkCode == VK_DUMMY as u32;
        if !target.0.is_null() && !injected {
            let vk = k.vkCode as u16;
            let post = |m: u32| {
                let _ = PostMessageW(Some(target), m, WPARAM(0), LPARAM(0));
            };
            if vk == VK_TAB.0 && (alt || open) {
                if down {
                    let shift = GetAsyncKeyState(VK_SHIFT.0 as i32) < 0;
                    post(if shift { WM_PREV } else { WM_NEXT });
                }
                return LRESULT(1);
            }
            if open {
                let nav = match vk {
                    v if v == VK_ESCAPE.0 => Some(WM_CANCEL),
                    v if v == VK_RIGHT.0 => Some(WM_NEXT),
                    v if v == VK_LEFT.0 || v == VK_OEM_3.0 => Some(WM_PREV),
                    v if v == VK_RETURN.0 => Some(WM_COMMIT),
                    VK_Q => Some(WM_QUIT_APP),
                    VK_H => Some(WM_HIDE_APP),
                    _ => None,
                };
                if let Some(m) = nav {
                    if down {
                        post(m);
                    }
                    return LRESULT(1);
                }
                if up && (vk == VK_LMENU.0 || vk == VK_RMENU.0) {
                    post(WM_COMMIT);
                }
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

fn suppress_alt_menu() {
    unsafe {
        keybd_event(VK_DUMMY, 0, KEYBD_EVENT_FLAGS(0), 0);
        keybd_event(VK_DUMMY, 0, KEYEVENTF_KEYUP, 0);
    }
}

fn app_name(w: &nacre_core::dock::items::Win) -> String {
    if !w.name.is_empty() {
        w.name.clone()
    } else {
        nacre_core::dock::items::name_from_path(&w.path)
    }
}

fn collect() -> Vec<Entry> {
    let wins = enumerate::list(std::process::id());
    let mut apps: Vec<(i64, Entry)> = Vec::new();
    for w in &wins {
        let key = w.path.to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        let rec = tracker::recent(w.hwnd);
        if let Some(e) = apps.iter_mut().find(|(_, e)| e.path.to_ascii_lowercase() == key) {
            if rec > e.0 {
                e.0 = rec;
                e.1.hwnd = w.hwnd;
            }
        } else {
            apps.push((rec, Entry { path: w.path.clone(), name: app_name(w), hwnd: w.hwnd }));
        }
    }
    apps.sort_by(|a, b| b.0.cmp(&a.0));
    apps.into_iter().map(|(_, e)| e).take(MAX_APPS).collect()
}

fn windows_of(path: &str) -> Vec<isize> {
    let key = path.to_ascii_lowercase();
    enumerate::list(std::process::id()).into_iter().filter(|w| w.path.to_ascii_lowercase() == key).map(|w| w.hwnd).collect()
}

fn recolor(b: &icons::Bitmap, rgb: [u8; 3]) -> Vec<u8> {
    let mut out = b.bgra.clone();
    for px in out.chunks_exact_mut(4) {
        let a = px[3] as u32;
        px[0] = (rgb[2] as u32 * a / 255) as u8;
        px[1] = (rgb[1] as u32 * a / 255) as u8;
        px[2] = (rgb[0] as u32 * a / 255) as u8;
    }
    out
}

fn styled_icon(path: &str, px: i32) -> Option<icons::Bitmap> {
    let mut b = icons::load(path, px)?;
    if !icons::is_themed(path) {
        let p = prefs::get();
        if p.dock_plates && b.w == b.h {
            b.bgra = nacre_core::iconplate::plate(&b.bgra, b.w);
        }
        b.bgra = nacre_core::iconplate::style_icon_v(&b.bgra, p.icon_style, p.icon_tint, prefs::icon_dark());
    }
    Some(b)
}

fn clip_round(comp: &Compositor, v: &SpriteVisual, size: Vector2, r: f32) -> windows_core::Result<()> {
    let g = comp.CreateRoundedRectangleGeometry()?;
    g.SetSize(size)?;
    g.SetCornerRadius(Vector2 { X: r, Y: r })?;
    let cl = comp.CreateGeometricClip()?;
    cl.SetGeometry(&g)?;
    v.SetClip(&cl)
}

impl Ctl {
    fn close(&mut self) {
        OPEN.store(false, Ordering::Relaxed);
        self.drop_ui();
    }

    fn drop_ui(&mut self) {
        if let Some(ui) = self.ui.take() {
            let h = ui.popup.hwnd;
            drop(ui);
            unsafe {
                let _ = DestroyWindow(h);
            }
        }
    }

    fn open(&mut self, backwards: bool) {
        let entries = collect();
        if entries.is_empty() {
            return;
        }
        suppress_alt_menu();
        let n = entries.len();
        let sel = if n == 1 { 0 } else if backwards { n - 1 } else { 1 };
        self.show(entries, sel, true);
    }

    fn show(&mut self, entries: Vec<Entry>, sel: usize, animate: bool) {
        if self.gfx.is_none() {
            self.gfx = Gfx::shared(&self.comp).ok();
        }
        match self.build(entries, animate) {
            Ok(ui) => {
                OPEN.store(true, Ordering::Relaxed);
                self.ui = Some(ui);
                self.select(sel, false);
            }
            Err(e) => {
                OPEN.store(false, Ordering::Relaxed);
                nacre_platform::log::line("switcher", &format!("gagal: {e}"));
            }
        }
    }

    fn build(&self, entries: Vec<Entry>, animate: bool) -> windows_core::Result<Ui> {
        let comp = &self.comp;
        let gfx = self.gfx.as_ref().ok_or_else(|| windows_core::Error::from(windows::Win32::Foundation::E_FAIL))?;
        let look = prefs::look(prefs::Role::Themed);
        let mut look = look;
        look.blur_sigma = look.blur_sigma.max(24.0);
        let base = if look.dark { nacre_ui::glass::GlassParams::dark((10.0, 10.0), RADIUS) } else { nacre_ui::glass::GlassParams::light((10.0, 10.0), RADIUS) };
        let popup = Popup::new(comp, w!("AbraSwitcher"), Some(sw_proc), WINDOW_EX_STYLE(0), prefs::apply(base, &look))?;
        let dpi = unsafe { GetDpiForWindow(popup.hwnd) }.max(96) as f32 / 96.0;
        let (sw, sh) = nacre_platform::sys::screen_size();

        let n = entries.len() as f32;
        let total_w = 2.0 * PAD_X + n * CELL + (n - 1.0).max(0.0) * GAP;
        let total_h = PAD_TOP + CELL + LABEL_H;
        let k = ((sw as f32 - 80.0 * dpi) / (total_w * dpi)).min(1.0);
        let u = dpi * k;
        let (pw, ph) = ((total_w * u).round(), (total_h * u).round());
        let mut popup = popup;
        popup.set_rect(((sw as f32 - pw) / 2.0) as i32, ((sh as f32 - ph) / 2.0) as i32, pw as i32, ph as i32)?;
        popup.panel.set_radius(RADIUS * u)?;
        let kids = popup.content.Children()?;

        let hl_color = if look.dark { Color { A: 56, R: 255, G: 255, B: 255 } } else { Color { A: 30, R: 0, G: 0, B: 0 } };
        let highlight = comp.CreateSpriteVisual()?;
        let cell = Vector2 { X: CELL * u, Y: CELL * u };
        highlight.SetSize(cell)?;
        highlight.SetBrush(&comp.CreateColorBrushWithColor(hl_color)?)?;
        clip_round(comp, &highlight, cell, SEL_RADIUS * u)?;
        kids.InsertAtTop(&highlight)?;

        let px = (ICON * u).round() as i32;
        let mut cells = Vec::with_capacity(entries.len());
        let mut x = PAD_X * u;
        for e in &entries {
            let (cx, cy) = (x, PAD_TOP * u);
            if let Some(b) = styled_icon(&e.path, px) {
                let surf = gfx.surface_from_bgra(b.w, b.h, &b.bgra)?;
                let br = comp.CreateSurfaceBrushWithSurface(&surf)?;
                br.SetStretch(CompositionStretch::Uniform)?;
                let v = comp.CreateSpriteVisual()?;
                v.SetBrush(&br)?;
                let s = px as f32;
                v.SetSize(Vector2 { X: s, Y: s })?;
                v.SetOffset(Vector3 { X: (cx + (CELL * u - s) / 2.0).round(), Y: (cy + (CELL * u - s) / 2.0).round(), Z: 0.0 })?;
                kids.InsertAtTop(&v)?;
            }
            cells.push((cx, cy, CELL * u, CELL * u));
            x += (CELL + GAP) * u;
        }

        let label = comp.CreateContainerVisual()?;
        kids.InsertAtTop(&label)?;

        popup.show();
        if animate {
            motion::pop_in(comp, popup.root(), Vector3 { X: pw / 2.0, Y: ph / 2.0, Z: 0.0 }, 0.96, 1.0, 0, 140)?;
        }
        Ok(Ui { popup, entries, sel: 0, cells, highlight, label, u, pw, fg: look.fg })
    }

    fn step(&mut self, delta: i32) {
        let Some(ui) = self.ui.as_ref() else { return };
        let n = ui.entries.len() as i32;
        if n == 0 {
            return;
        }
        let next = ((ui.sel as i32 + delta).rem_euclid(n)) as usize;
        self.select(next, true);
    }

    fn select(&mut self, i: usize, animate: bool) {
        let comp = self.comp.clone();
        let Some(gfx) = self.gfx.as_ref() else { return };
        let Some(ui) = self.ui.as_mut() else { return };
        let Some(&(x, y, _, h)) = ui.cells.get(i) else { return };
        ui.sel = i;
        let to = Vector3 { X: x, Y: y, Z: 0.0 };
        if animate {
            let from = ui.highlight.Offset().unwrap_or(to);
            let _ = motion::vec3(&comp, &ui.highlight, "Offset", from, to, motion::SWITCHER_MOVE_MS);
        }
        let _ = ui.highlight.SetOffset(to);

        // Nama app di bawah ikon terpilih, dijepit di dalam panel.
        if let Ok(kids) = ui.label.Children() {
            let _ = kids.RemoveAll();
            let u = ui.u;
            let name = &ui.entries[i].name;
            let style = Style { px: (14.0 * u).round() as i32, weight: 500 };
            let (tw, th) = measure(name, &style);
            let tw = (tw + 2).min((ui.pw - 16.0 * u) as i32).max(8);
            if let Some(bmp) = render(name, &style, tw, th) {
                if let Ok(surf) = gfx.surface_from_bgra(bmp.w, bmp.h, &recolor(&bmp, ui.fg)) {
                    if let (Ok(br), Ok(v)) = (comp.CreateSurfaceBrushWithSurface(&surf), comp.CreateSpriteVisual()) {
                        let _ = br.SetStretch(CompositionStretch::None);
                        let _ = v.SetBrush(&br);
                        let _ = v.SetSize(Vector2 { X: tw as f32, Y: th as f32 });
                        let cx = x + ui.cells[i].2 / 2.0;
                        let lx = (cx - tw as f32 / 2.0).clamp(8.0 * u, ui.pw - 8.0 * u - tw as f32).round();
                        let ly = (y + h + (LABEL_H * u - th as f32) / 2.0 - 4.0 * u).round();
                        let _ = v.SetOffset(Vector3 { X: lx, Y: ly, Z: 0.0 });
                        let _ = kids.InsertAtTop(&v);
                        if animate {
                            let _ = motion::scalar(&comp, &v, "Opacity", 0.0, 1.0, 120);
                        }
                    }
                }
            }
        }
    }

    fn hit(&self, x: f32, y: f32) -> Option<usize> {
        let ui = self.ui.as_ref()?;
        ui.cells.iter().position(|&(cx, cy, w, h)| x >= cx && x < cx + w && y >= cy && y < cy + h)
    }

    fn commit(&mut self) {
        let pick = self.ui.as_ref().and_then(|u| u.entries.get(u.sel)).cloned();
        self.close();
        let Some(e) = pick else { return };
        if crate::stage::is_active() {
            crate::stage::bring_app(&e.path);
        } else if !focus::force_foreground(e.hwnd) {
            if let Some(&h) = windows_of(&e.path).first() {
                focus::force_foreground(h);
            }
        }
    }

    /// Q: tutup semua jendela app terpilih, H: minimalkan. Daftar lalu dibangun ulang.
    fn act_on_selected(&mut self, quit: bool) {
        let Some(ui) = self.ui.as_ref() else { return };
        let sel = ui.sel;
        let mut entries = ui.entries.clone();
        let target = entries.remove(sel);
        for h in windows_of(&target.path) {
            if quit {
                focus::request_close(h);
            } else {
                unsafe {
                    let _ = ShowWindow(HWND(h as *mut _), SW_MINIMIZE);
                }
            }
        }
        self.drop_ui();
        if entries.is_empty() {
            OPEN.store(false, Ordering::Relaxed);
            return;
        }
        let next = sel.min(entries.len() - 1);
        self.show(entries, next, false);
    }
}

unsafe extern "system" fn ctl_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        prefs::WM_PREFS => {
            sync();
            LRESULT(0)
        }
        WM_INSTALL => {
            install();
            LRESULT(0)
        }
        WM_NEXT | WM_PREV => {
            with(|c| {
                if c.ui.is_some() {
                    c.step(if msg == WM_NEXT { 1 } else { -1 });
                } else {
                    c.open(msg == WM_PREV);
                }
            });
            LRESULT(0)
        }
        WM_COMMIT => {
            with(|c| c.commit());
            LRESULT(0)
        }
        WM_CANCEL => {
            with(|c| c.close());
            LRESULT(0)
        }
        WM_QUIT_APP | WM_HIDE_APP => {
            with(|c| c.act_on_selected(msg == WM_QUIT_APP));
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn mouse_xy(lp: LPARAM) -> (f32, f32) {
    ((lp.0 & 0xFFFF) as i16 as f32, ((lp.0 >> 16) & 0xFFFF) as i16 as f32)
}

unsafe extern "system" fn sw_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE => {
            let (x, y) = mouse_xy(lp);
            with(|c| {
                if let Some(i) = c.hit(x, y) {
                    if c.ui.as_ref().is_some_and(|u| u.sel != i) {
                        c.select(i, true);
                    }
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = mouse_xy(lp);
            with(|c| {
                if let Some(i) = c.hit(x, y) {
                    c.select(i, false);
                    c.commit();
                }
            });
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
