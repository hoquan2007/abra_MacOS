use windows::UI::Composition::Compositor;

pub fn init(_comp: &Compositor) -> windows_core::Result<()> {
    Ok(())
}

pub fn shutdown() {}

/// Kisi App kini memakai menu Apps (kolom cari, chip kelompok, kisi ikon).
pub fn toggle() {
    crate::appmenu::toggle();
}
