//! Menu Apps ABRA: pengganti menu Start saat tombol Windows ditekan lalu dilepas
//! sendirian. Kolom cari di atas, chip kelompok app di bawahnya, lalu kisi ikon
//! di atas panel kaca. Mengetik langsung menyaring, Enter membuka app, Esc menutup.
//! Kombinasi Win+tombol lain (Win+E, Win+D, Win+Tab, ...) tetap diteruskan ke Windows.

use crate::popup::Popup;
use crate::{motion, paths, prefs};
use nacre_core::appcat::{self, Cat};
use nacre_core::search;
use nacre_core::symbols::{self, Sym};
use nacre_platform::apps::{self, App};
use nacre_platform::icons::{self, Bitmap};
use nacre_platform::text::{measure, render, Style};
use nacre_platform::{launch, log, sys};
use nacre_ui::gfx::Gfx;
use nacre_ui::glass::GlassParams;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use windows::core::w;
use windows::UI::Color;
use windows::UI::Composition::{CompositionDrawingSurface, CompositionStretch, Compositor, ContainerVisual};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VIRTUAL_KEY,
    VK_BACK, VK_CONTROL, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_LWIN, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_RWIN, VK_SHIFT,
    VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows_numerics::{Vector2, Vector3};

const WM_TOGGLE: u32 = WM_APP + 140;
const WM_APPS_READY: u32 = WM_APP + 141;
const WM_CLOSE_MENU: u32 = WM_APP + 142;
const T_FINISH: usize = 1;

/// Tombol virtual tak terpakai, disisipkan di antara Win turun dan Win naik
/// agar Windows menganggapnya kombinasi dan tidak membuka menu Start.
const VK_MASK: u16 = 0xE8;
/// Penanda pada input yang dikirim modul ini, agar hook tidak memprosesnya lagi.
const MARK: usize = 0xAB_4D45;

const OPEN_MS: u32 = 220;
const CLOSE_MS: u32 = 140;

type Rect = (f32, f32, f32, f32);

static HOOK: std::sync::Mutex<Option<nacre_platform::hookthread::HookThread>> = std::sync::Mutex::new(None);
static TARGET: AtomicIsize = AtomicIsize::new(0);
static WIN_HELD: AtomicU32 = AtomicU32::new(0);
static WIN_COMBO: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, PartialEq)]
enum Chip {
    All,
    Suggested,
    Cat(Cat),
}

impl Chip {
    fn title(self) -> &'static str {
        match self {
            Chip::All => prefs::tr("Semua"),
            Chip::Suggested => prefs::tr("Saran"),
            Chip::Cat(c) => prefs::tr(c.title()),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Hit {
    Chip(usize),
    Cell(usize),
    Panel,
}

struct Ui {
    popup: Popup,
    layer: ContainerVisual,
    d: f32,
    w: f32,
    h: f32,
    dark: bool,
    fg: [u8; 3],
    query: String,
    chips: Vec<Chip>,
    chip: usize,
    list: Vec<usize>,
    sel: usize,
    scroll: usize,
    cols: usize,
    vis_rows: usize,
    hits: Vec<(Rect, Hit)>,
    mouse: (i32, i32),
}

struct Ctl {
    hwnd: HWND,
    comp: Compositor,
    gfx: Option<Gfx>,
    apps: Vec<App>,
    cats: Vec<Cat>,
    texts: HashMap<String, (CompositionDrawingSurface, i32, i32)>,
    icons: HashMap<String, Option<CompositionDrawingSurface>>,
    ui: Option<Ui>,
    closing: Option<Popup>,
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
        let class = w!("AbraAppMenuCtl");
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(ctl_proc), hInstance: hi, lpszClassName: class, ..Default::default() });
        let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), class, w!("abra-appmenu"), WS_POPUP, 0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(hi), None)?;
        TARGET.store(hwnd.0 as isize, Ordering::Relaxed);
        let ctl = Ctl { hwnd, comp: comp.clone(), gfx: None, apps: vec![], cats: vec![], texts: HashMap::new(), icons: HashMap::new(), ui: None, closing: None };
        CTL.with(|c| *c.borrow_mut() = Some(ctl));
        prefs::listen(hwnd);
    }
    sync();
    Ok(())
}

pub fn shutdown() {
    uninstall();
}

/// Buka/tutup menu Apps (juga dipakai Deck, sudut aktif, dan bilah menu).
pub fn toggle() {
    let h = TARGET.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), WM_TOGGLE, WPARAM(0), LPARAM(0));
        }
    }
}

fn sync() {
    if prefs::get().win_menu {
        install();
    } else {
        uninstall();
    }
}

fn install() {
    let mut g = HOOK.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        *g = nacre_platform::hookthread::HookThread::start(nacre_platform::hookthread::Kind::Keyboard, Some(ll_proc));
        if g.is_none() {
            log::line("appmenu", "hook keyboard gagal dipasang; tombol Windows tetap membuka Start");
        }
    }
}

fn uninstall() {
    let h = HOOK.lock().unwrap_or_else(|e| e.into_inner()).take();
    drop(h);
    WIN_HELD.store(0, Ordering::Relaxed);
}

fn key_input(vk: u16, up: bool, extended: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: MARK } } }
}

unsafe extern "system" fn ll_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        // Hanya abaikan input yang kita kirim sendiri; Win dari keyboard layar
        // atau alat remote (injected) tetap diperlakukan seperti tombol asli.
        let ours = k.dwExtraInfo == MARK;
        let m = wp.0 as u32;
        let down = m == WM_KEYDOWN || m == WM_SYSKEYDOWN;
        let up = m == WM_KEYUP || m == WM_SYSKEYUP;
        let vk = k.vkCode;
        let is_win = vk == VK_LWIN.0 as u32 || vk == VK_RWIN.0 as u32;
        if !ours {
            if is_win {
                if down {
                    // Pengulangan otomatis mengirim "turun" berkali-kali; catat sekali saja.
                    if WIN_HELD.load(Ordering::Relaxed) == 0 {
                        WIN_HELD.store(vk, Ordering::Relaxed);
                        WIN_COMBO.store(false, Ordering::Relaxed);
                    }
                } else if up && WIN_HELD.load(Ordering::Relaxed) == vk {
                    WIN_HELD.store(0, Ordering::Relaxed);
                    if !WIN_COMBO.load(Ordering::Relaxed) {
                        // Telan Win naik yang asli, lalu kirim ulang dengan tombol samaran
                        // di depannya: Windows melihat kombinasi dan tidak membuka Start.
                        let seq = [key_input(VK_MASK, false, false), key_input(VK_MASK, true, false), key_input(vk as u16, true, true)];
                        SendInput(&seq, std::mem::size_of::<INPUT>() as i32);
                        let target = TARGET.load(Ordering::Relaxed);
                        if target != 0 {
                            let _ = PostMessageW(Some(HWND(target as *mut _)), WM_TOGGLE, WPARAM(0), LPARAM(0));
                        }
                        return LRESULT(1);
                    }
                }
            } else if down && WIN_HELD.load(Ordering::Relaxed) != 0 {
                WIN_COMBO.store(true, Ordering::Relaxed);
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

fn recolor(b: &Bitmap, rgb: [u8; 3]) -> Vec<u8> {
    let mut out = b.bgra.clone();
    for px in out.chunks_exact_mut(4) {
        let a = px[3] as u32;
        px[0] = (rgb[2] as u32 * a / 255) as u8;
        px[1] = (rgb[1] as u32 * a / 255) as u8;
        px[2] = (rgb[0] as u32 * a / 255) as u8;
    }
    out
}

fn path_key(p: &str) -> String {
    p.replace('/', "\\").to_lowercase()
}

fn hit_rect(r: Rect, x: f32, y: f32) -> bool {
    x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3
}

struct Paint<'a> {
    comp: &'a Compositor,
    gfx: &'a Gfx,
    texts: &'a mut HashMap<String, (CompositionDrawingSurface, i32, i32)>,
    icons: &'a mut HashMap<String, Option<CompositionDrawingSurface>>,
}

impl Paint<'_> {
    fn sprite(&self, parent: &ContainerVisual, s: &CompositionDrawingSurface, x: f32, y: f32, w: f32, h: f32, op: f32) {
        let go = || -> windows_core::Result<()> {
            let b = self.comp.CreateSurfaceBrushWithSurface(s)?;
            b.SetStretch(CompositionStretch::Uniform)?;
            let v = self.comp.CreateSpriteVisual()?;
            v.SetBrush(&b)?;
            v.SetSize(Vector2 { X: w, Y: h })?;
            v.SetOffset(Vector3 { X: x.round(), Y: y.round(), Z: 0.0 })?;
            v.SetOpacity(op)?;
            parent.Children()?.InsertAtTop(&v)
        };
        let _ = go();
    }

    fn rect(&self, parent: &ContainerVisual, r: Rect, radius: f32, c: Color) {
        let go = || -> windows_core::Result<()> {
            let v = self.comp.CreateSpriteVisual()?;
            v.SetSize(Vector2 { X: r.2, Y: r.3 })?;
            v.SetOffset(Vector3 { X: r.0, Y: r.1, Z: 0.0 })?;
            v.SetBrush(&self.comp.CreateColorBrushWithColor(c)?)?;
            if radius > 0.0 {
                let g = self.comp.CreateRoundedRectangleGeometry()?;
                g.SetSize(Vector2 { X: r.2, Y: r.3 })?;
                g.SetCornerRadius(Vector2 { X: radius, Y: radius })?;
                let clip = self.comp.CreateGeometricClip()?;
                clip.SetGeometry(&g)?;
                v.SetClip(&clip)?;
            }
            parent.Children()?.InsertAtTop(&v)
        };
        let _ = go();
    }

    /// Teks dengan tepi kiri `x` dan titik tengah vertikal `cy`; mengembalikan lebarnya.
    fn text(&mut self, parent: &ContainerVisual, s: &str, x: f32, cy: f32, px: i32, weight: i32, rgb: [u8; 3], op: f32, max_w: f32) -> f32 {
        if s.is_empty() {
            return 0.0;
        }
        let key = format!("{px}|{weight}|{rgb:?}|{}|{s}", max_w as i32);
        let (surf, w, h) = match self.texts.get(&key).cloned() {
            Some(v) => v,
            None => {
                let style = Style { px, weight };
                let (tw, th) = measure(s, &style);
                let tw = (tw + 2).min(max_w as i32).max(2);
                let Some(bmp) = render(s, &style, tw, th) else { return 0.0 };
                let Ok(sf) = self.gfx.surface_from_bgra(bmp.w, bmp.h, &recolor(&bmp, rgb)) else { return 0.0 };
                if self.texts.len() > 400 {
                    self.texts.clear();
                }
                let v = (sf, tw, th);
                self.texts.insert(key, v.clone());
                v
            }
        };
        self.sprite(parent, &surf, x, cy - h as f32 / 2.0, w as f32, h as f32, op);
        w as f32
    }

    fn text_w(s: &str, px: i32, weight: i32) -> f32 {
        measure(s, &Style { px, weight }).0 as f32 + 2.0
    }

    fn sym(&mut self, parent: &ContainerVisual, s: Sym, cx: f32, cy: f32, size: f32, rgb: [u8; 3], op: f32) {
        let n = size.round().max(8.0) as u32;
        let key = format!("sym|{s:?}|{n}|{rgb:?}");
        let (surf, w, h) = match self.texts.get(&key).cloned() {
            Some(v) => v,
            None => {
                let data = symbols::render(s, n, rgb);
                let Some((cd, cw, ch)) = symbols::crop_ink(&data, n, n) else { return };
                let Ok(sf) = self.gfx.surface_from_bgra(cw, ch, &cd) else { return };
                let v = (sf, cw as i32, ch as i32);
                self.texts.insert(key, v.clone());
                v
            }
        };
        self.sprite(parent, &surf, cx - w as f32 / 2.0, cy - h as f32 / 2.0, w as f32, h as f32, op);
    }

    /// Ikon app bergaya sama seperti di Deck (pelat warna, gaya, rona).
    fn icon(&mut self, path: &str, px: i32) -> Option<CompositionDrawingSurface> {
        let key = format!("{px}|{path}");
        if let Some(c) = self.icons.get(&key) {
            return c.clone();
        }
        let p = prefs::get();
        let s = icons::load(path, px).and_then(|mut b| {
            if !icons::is_themed(path) {
                if p.dock_plates && b.w == b.h {
                    b.bgra = nacre_core::iconplate::plate(&b.bgra, b.w);
                }
                b.bgra = nacre_core::iconplate::style_icon_v(&b.bgra, p.icon_style, p.icon_tint, prefs::icon_dark());
            }
            self.gfx.surface_from_bgra(b.w, b.h, &b.bgra).ok()
        });
        if self.icons.len() > 600 {
            self.icons.clear();
        }
        self.icons.insert(key, s.clone());
        s
    }
}

// Ukuran logis (dikali skala layar).
const PAD: f32 = 28.0;
const SEARCH_W: f32 = 380.0;
const SEARCH_H: f32 = 38.0;
const TOP: f32 = 26.0;
const CHIP_H: f32 = 28.0;
const CELL_W: f32 = 116.0;
const CELL_H: f32 = 112.0;
const ICON: f32 = 64.0;

impl Ui {
    fn grid_top(&self) -> f32 {
        (TOP + SEARCH_H + 16.0 + CHIP_H + 22.0) * self.d
    }

    fn rows(&self) -> usize {
        self.list.len().div_ceil(self.cols.max(1))
    }

    fn max_scroll(&self) -> usize {
        self.rows().saturating_sub(self.vis_rows)
    }

    fn keep_visible(&mut self) {
        let row = self.sel / self.cols.max(1);
        if row < self.scroll {
            self.scroll = row;
        } else if row >= self.scroll + self.vis_rows {
            self.scroll = row + 1 - self.vis_rows;
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }
}

impl Ctl {
    fn suggestions(&self) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        for f in ["pins.txt", "recents.txt"] {
            let text = std::fs::read_to_string(paths::config_dir().join(f)).unwrap_or_default();
            for line in text.lines() {
                let p = path_key(line.split('\t').next().unwrap_or("").trim());
                if p.is_empty() {
                    continue;
                }
                if let Some(i) = self.apps.iter().position(|a| path_key(&a.path) == p) {
                    if !out.contains(&i) {
                        out.push(i);
                    }
                }
                if out.len() >= 21 {
                    return out;
                }
            }
        }
        out
    }

    fn set_apps(&mut self, apps: Vec<App>) {
        self.cats = apps.iter().map(|a| appcat::guess(&a.name, &a.path)).collect();
        self.apps = apps;
    }

    fn chips(&self) -> Vec<Chip> {
        let mut v = vec![Chip::All];
        if !self.suggestions().is_empty() {
            v.push(Chip::Suggested);
        }
        for c in appcat::ALL {
            if self.cats.contains(&c) {
                v.push(Chip::Cat(c));
            }
        }
        v
    }

    fn rebuild(&mut self) {
        let chips = self.chips();
        let sugg = self.suggestions();
        let Some(ui) = self.ui.as_mut() else { return };
        let cur = ui.chips.get(ui.chip).copied().unwrap_or(Chip::All);
        ui.chip = chips.iter().position(|c| *c == cur).unwrap_or(0);
        ui.chips = chips;
        let base: Vec<usize> = match ui.chips[ui.chip] {
            Chip::All => (0..self.apps.len()).collect(),
            Chip::Suggested => sugg,
            Chip::Cat(c) => (0..self.apps.len()).filter(|&i| self.cats[i] == c).collect(),
        };
        let q = ui.query.trim();
        ui.list = if q.is_empty() {
            base
        } else {
            let names: Vec<String> = base.iter().map(|&i| self.apps[i].name.clone()).collect();
            search::rank(q, &names, 1000).into_iter().map(|k| base[k]).collect()
        };
        ui.sel = 0;
        ui.scroll = 0;
    }

    fn open(&mut self) {
        if self.ui.is_some() {
            return;
        }
        self.finish_closing();
        if self.gfx.is_none() {
            self.gfx = Gfx::shared(&self.comp).ok();
        }
        if self.apps.is_empty() {
            self.set_apps(apps::snapshot());
        }
        match self.build() {
            Ok(ui) => {
                self.ui = Some(ui);
                self.rebuild();
                self.render();
                if let Some(ui) = self.ui.as_ref() {
                    ui.popup.show_active();
                    nacre_platform::wins::focus::force_foreground(ui.popup.hwnd.0 as isize);
                    let c = Vector3 { X: ui.w / 2.0, Y: ui.h / 2.0, Z: 0.0 };
                    let _ = motion::pop_in(&self.comp, ui.popup.root(), c, 0.94, 1.006, 0, OPEN_MS);
                }
                let h = self.hwnd.0 as isize;
                apps::refresh(move || unsafe {
                    let _ = PostMessageW(Some(HWND(h as *mut _)), WM_APPS_READY, WPARAM(0), LPARAM(0));
                });
            }
            Err(e) => log::line("appmenu", &format!("gagal: {e}")),
        }
    }

    fn build(&self) -> windows_core::Result<Ui> {
        let comp = &self.comp;
        let mon = nacre_platform::monitors::under_cursor();
        let (sw, sh) = mon.area.size();
        let d = mon.scale();
        let mut look = prefs::look(prefs::Role::Themed);
        look.blur_sigma = look.blur_sigma.max(30.0);
        let radius = 30.0 * d;
        let base = if look.dark { GlassParams::dark((10.0, 10.0), radius) } else { GlassParams::light((10.0, 10.0), radius) };
        let mut popup = Popup::new_with(comp, w!("AbraAppMenu"), Some(menu_proc), WINDOW_EX_STYLE(0), prefs::apply(base, &look), true)?;
        let tier = {
            let c = sys::conditions();
            if !c.transparency || c.battery_saver || c.remote_session { nacre_ui::glass::Tier::Solid } else { nacre_ui::glass::Tier::Full }
        };
        let _ = popup.panel.set_tier(tier);
        let w = (1040.0 * d).min(sw as f32 * 0.82).round();
        let h = (720.0 * d).min(sh as f32 * 0.82).round();
        let x = mon.area.x + ((sw as f32 - w) / 2.0) as i32;
        let y = mon.area.y + ((sh as f32 - h) / 2.0) as i32;
        popup.set_rect(x, y, w as i32, h as i32)?;
        popup.panel.set_radius(radius)?;
        let layer = comp.CreateContainerVisual()?;
        popup.content.Children()?.InsertAtTop(&layer)?;
        let cols = (((w - 2.0 * PAD * d) / (CELL_W * d)).floor() as usize).max(1);
        let grid_top = (TOP + SEARCH_H + 16.0 + CHIP_H + 22.0) * d;
        let vis_rows = (((h - grid_top - 18.0 * d) / (CELL_H * d)).floor() as usize).max(1);
        Ok(Ui {
            popup,
            layer,
            d,
            w,
            h,
            dark: look.dark,
            fg: look.fg,
            query: String::new(),
            chips: vec![Chip::All],
            chip: 0,
            list: vec![],
            sel: 0,
            scroll: 0,
            cols,
            vis_rows,
            hits: vec![],
            mouse: (-1, -1),
        })
    }

    fn render(&mut self) {
        let Ctl { comp, gfx, texts, icons, ui, apps, .. } = self;
        let (Some(gfx), Some(ui)) = (gfx.as_ref(), ui.as_mut()) else { return };
        let mut p = Paint { comp, gfx, texts, icons };
        let layer = ui.layer.clone();
        if let Ok(k) = layer.Children() {
            let _ = k.RemoveAll();
        }
        let d = ui.d;
        let fg = ui.fg;
        let soft = |a: u8| if ui.dark { Color { A: a, R: 255, G: 255, B: 255 } } else { Color { A: a, R: 0, G: 0, B: 0 } };
        let mut hits: Vec<(Rect, Hit)> = vec![((0.0, 0.0, ui.w, ui.h), Hit::Panel)];

        // Kolom cari.
        let sr = (((ui.w - SEARCH_W * d) / 2.0).round(), (TOP * d).round(), (SEARCH_W * d).round(), (SEARCH_H * d).round());
        p.rect(&layer, sr, sr.3 / 2.0, soft(if ui.dark { 30 } else { 16 }));
        let cy = sr.1 + sr.3 / 2.0;
        p.sym(&layer, Sym::Search, sr.0 + 20.0 * d, cy, 15.0 * d, fg, 0.6);
        let tx = sr.0 + 36.0 * d;
        let px = (15.0 * d).round() as i32;
        let max_tw = sr.2 - 52.0 * d;
        if ui.query.is_empty() {
            p.text(&layer, prefs::tr("Cari app"), tx, cy, px, 400, fg, 0.5, max_tw);
            p.rect(&layer, (tx - 1.0 * d, cy - 9.0 * d, (1.5 * d).max(1.0), 18.0 * d), 0.0, prefs::accent_c(230));
        } else {
            let tw = p.text(&layer, &ui.query, tx, cy, px, 400, fg, 0.95, max_tw);
            p.rect(&layer, (tx + tw, cy - 9.0 * d, (1.5 * d).max(1.0), 18.0 * d), 0.0, prefs::accent_c(230));
        }

        // Chip kelompok.
        let chip_px = (13.0 * d).round() as i32;
        let pad_x = 14.0 * d;
        let gap = 8.0 * d;
        let widths: Vec<f32> = ui.chips.iter().map(|c| Paint::text_w(c.title(), chip_px, 500) + 2.0 * pad_x).collect();
        let total: f32 = widths.iter().sum::<f32>() + gap * (widths.len().saturating_sub(1)) as f32;
        let mut x = ((ui.w - total) / 2.0).round();
        let chip_y = ((TOP + SEARCH_H + 16.0) * d).round();
        let ch = (CHIP_H * d).round();
        for (i, c) in ui.chips.iter().enumerate() {
            let r = (x, chip_y, widths[i].round(), ch);
            let on = i == ui.chip;
            if on {
                p.rect(&layer, r, ch / 2.0, prefs::accent_c(if ui.dark { 210 } else { 230 }));
            } else {
                p.rect(&layer, r, ch / 2.0, soft(if ui.dark { 22 } else { 12 }));
            }
            let col = if on { [255, 255, 255] } else { fg };
            p.text(&layer, c.title(), x + pad_x, chip_y + ch / 2.0, chip_px, 500, col, if on { 1.0 } else { 0.82 }, widths[i]);
            hits.push((r, Hit::Chip(i)));
            x += widths[i] + gap;
        }

        // Kisi ikon.
        let top = ui.grid_top();
        let (cw, chh) = (CELL_W * d, CELL_H * d);
        let grid_w = cw * ui.cols as f32;
        let left = ((ui.w - grid_w) / 2.0).round();
        let icon_px = (ICON * d).round() as i32;
        let label_px = (12.5 * d).round() as i32;
        if ui.list.is_empty() {
            let msg = prefs::tr("Tidak ada aplikasi");
            let tw = Paint::text_w(msg, (15.0 * d) as i32, 500);
            p.text(&layer, msg, (ui.w - tw) / 2.0, top + 80.0 * d, (15.0 * d) as i32, 500, fg, 0.55, ui.w);
        }
        let first = ui.scroll * ui.cols;
        let last = (first + ui.vis_rows * ui.cols).min(ui.list.len());
        for (slot, li) in (first..last).enumerate() {
            let (r, c) = (slot / ui.cols, slot % ui.cols);
            let cell = (left + c as f32 * cw, top + r as f32 * chh, cw, chh);
            if li == ui.sel {
                let hl = (cell.0 + 6.0 * d, cell.1 + 2.0 * d, cell.2 - 12.0 * d, cell.3 - 4.0 * d);
                p.rect(&layer, hl, 18.0 * d, soft(if ui.dark { 40 } else { 22 }));
            }
            let app = &apps[ui.list[li]];
            let isz = icon_px as f32;
            let ix = cell.0 + (cw - isz) / 2.0;
            let iy = cell.1 + 10.0 * d;
            if let Some(s) = p.icon(&app.path, icon_px) {
                p.sprite(&layer, &s, ix, iy, isz, isz, 1.0);
            }
            let max_w = cw - 14.0 * d;
            let tw = Paint::text_w(&app.name, label_px, 400).min(max_w);
            p.text(&layer, &app.name, cell.0 + (cw - tw) / 2.0, iy + isz + 15.0 * d, label_px, 400, fg, 0.92, max_w);
            hits.push((cell, Hit::Cell(li)));
        }

        // Bilah gulir tipis bila daftar lebih panjang dari layar.
        let rows = ui.rows();
        if rows > ui.vis_rows {
            let track_h = ui.vis_rows as f32 * chh;
            let th = (track_h * ui.vis_rows as f32 / rows as f32).max(24.0 * d);
            let ty = top + (track_h - th) * ui.scroll as f32 / ui.max_scroll().max(1) as f32;
            p.rect(&layer, (ui.w - 12.0 * d, ty, 4.0 * d, th), 2.0 * d, soft(if ui.dark { 70 } else { 50 }));
        }
        ui.hits = hits;
    }

    fn hit(&self, x: f32, y: f32) -> Option<Hit> {
        let ui = self.ui.as_ref()?;
        ui.hits.iter().rev().find(|(r, _)| hit_rect(*r, x, y)).map(|(_, h)| *h)
    }

    fn close(&mut self) {
        let Some(ui) = self.ui.take() else { return };
        self.finish_closing();
        let c = Vector3 { X: ui.w / 2.0, Y: ui.h / 2.0, Z: 0.0 };
        let _ = motion::pop_out(&self.comp, ui.popup.root(), c, 0.96, CLOSE_MS);
        self.closing = Some(ui.popup);
        unsafe {
            SetTimer(Some(self.hwnd), T_FINISH, CLOSE_MS + 20, None);
        }
    }

    fn finish_closing(&mut self) {
        if let Some(p) = self.closing.take() {
            let h = p.hwnd;
            drop(p);
            unsafe {
                let _ = DestroyWindow(h);
            }
            sys::trim_later(5000);
        }
    }

    fn launch_sel(&mut self) {
        let path = self.ui.as_ref().and_then(|u| u.list.get(u.sel)).map(|&i| self.apps[i].path.clone());
        if let Some(p) = path {
            self.close();
            launch::open(&p);
        }
    }

    fn set_chip(&mut self, i: usize) {
        if let Some(ui) = self.ui.as_mut() {
            if i < ui.chips.len() && i != ui.chip {
                ui.chip = i;
                self.rebuild();
                self.render();
            }
        }
    }

    fn key(&mut self, vk: u16) -> bool {
        let Some(ui) = self.ui.as_mut() else { return false };
        let n = ui.list.len();
        let cols = ui.cols.max(1);
        let page = cols * ui.vis_rows;
        let mv = |sel: usize, delta: isize| -> usize {
            if n == 0 {
                0
            } else {
                (sel as isize + delta).clamp(0, n as isize - 1) as usize
            }
        };
        match VIRTUAL_KEY(vk) {
            VK_ESCAPE => {
                self.close();
                return true;
            }
            VK_RETURN => {
                self.launch_sel();
                return true;
            }
            VK_TAB => {
                let len = ui.chips.len();
                let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
                let next = if shift { (ui.chip + len - 1) % len } else { (ui.chip + 1) % len };
                self.set_chip(next);
                return true;
            }
            VK_BACK => {
                if ui.query.pop().is_some() {
                    self.rebuild();
                    self.render();
                }
                return true;
            }
            VK_LEFT => ui.sel = mv(ui.sel, -1),
            VK_RIGHT => ui.sel = mv(ui.sel, 1),
            VK_UP => ui.sel = mv(ui.sel, -(cols as isize)),
            VK_DOWN => ui.sel = mv(ui.sel, cols as isize),
            VK_PRIOR => ui.sel = mv(ui.sel, -(page as isize)),
            VK_NEXT => ui.sel = mv(ui.sel, page as isize),
            VK_HOME => ui.sel = 0,
            VK_END => ui.sel = n.saturating_sub(1),
            _ => return false,
        }
        ui.keep_visible();
        self.render();
        true
    }

    fn type_char(&mut self, ch: char) {
        let Some(ui) = self.ui.as_mut() else { return };
        if ui.query.chars().count() >= 60 {
            return;
        }
        ui.query.push(ch);
        self.rebuild();
        self.render();
    }

    fn wheel(&mut self, delta: i16) {
        let Some(ui) = self.ui.as_mut() else { return };
        let max = ui.max_scroll();
        let s = if delta > 0 { ui.scroll.saturating_sub(1) } else { (ui.scroll + 1).min(max) };
        if s != ui.scroll {
            ui.scroll = s;
            let cols = ui.cols.max(1);
            let first = s * cols;
            let last = (first + ui.vis_rows * cols).min(ui.list.len()).max(first + 1) - 1;
            ui.sel = ui.sel.clamp(first, last.max(first));
            self.render();
        }
    }

    fn hover(&mut self, x: i32, y: i32) {
        let changed = match self.ui.as_mut() {
            Some(ui) if ui.mouse != (x, y) => {
                ui.mouse = (x, y);
                true
            }
            _ => false,
        };
        if !changed {
            return;
        }
        if let Some(Hit::Cell(i)) = self.hit(x as f32, y as f32) {
            if let Some(ui) = self.ui.as_mut() {
                if ui.sel != i {
                    ui.sel = i;
                    self.render();
                }
            }
        }
    }

    fn click(&mut self, x: i32, y: i32) {
        match self.hit(x as f32, y as f32) {
            Some(Hit::Cell(i)) => {
                if let Some(ui) = self.ui.as_mut() {
                    ui.sel = i;
                }
                self.launch_sel();
            }
            Some(Hit::Chip(i)) => self.set_chip(i),
            _ => {}
        }
    }
}

unsafe extern "system" fn ctl_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TOGGLE => {
            with(|c| if c.ui.is_some() { c.close() } else { c.open() });
            LRESULT(0)
        }
        WM_APPS_READY => {
            with(|c| {
                let fresh = apps::snapshot();
                if fresh != c.apps {
                    c.set_apps(fresh);
                    if c.ui.is_some() {
                        c.rebuild();
                        c.render();
                    }
                }
            });
            LRESULT(0)
        }
        WM_TIMER if wp.0 == T_FINISH => {
            let _ = KillTimer(Some(hwnd), T_FINISH);
            with(|c| c.finish_closing());
            LRESULT(0)
        }
        m if m == prefs::WM_PREFS => {
            sync();
            // Tema/gaya ikon bisa berubah; menu tetap bisa dibuka dari Deck walau hook dilepas.
            with(|c| {
                c.icons.clear();
                c.texts.clear();
            });
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn mouse_xy(lp: LPARAM) -> (i32, i32) {
    ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32)
}

fn ctrl_down() -> bool {
    unsafe { GetKeyState(VK_CONTROL.0 as i32) < 0 }
}

unsafe extern "system" fn menu_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CHAR => {
            let c = wp.0 as u32;
            if c >= 32 && c != 127 && !ctrl_down() {
                if let Some(ch) = char::from_u32(c) {
                    with(|s| s.type_char(ch));
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if with(|s| s.key(wp.0 as u16)).unwrap_or(false) {
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        WM_MOUSEMOVE => {
            let (x, y) = mouse_xy(lp);
            with(|s| s.hover(x, y));
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = mouse_xy(lp);
            with(|s| s.click(x, y));
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp.0 >> 16) & 0xFFFF) as i16;
            with(|s| s.wheel(delta));
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if (wp.0 & 0xFFFF) == 0 {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE_MENU, WPARAM(0), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_CLOSE_MENU => {
            // Hanya tutup bila jendela ini masih menu yang terbuka (bukan sisa animasi tutup).
            with(|s| {
                if s.ui.as_ref().is_some_and(|u| u.popup.hwnd == hwnd) {
                    s.close();
                }
            });
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
