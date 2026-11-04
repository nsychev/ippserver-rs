//! The PNG icons served for `printer-icons`.
//!
//! A printer must have an icon as per IPP Everywhere Table 5.

#[derive(Debug, Clone, Copy)]
pub struct Icon {
    pub path: &'static str,
    pub size: u32,
    pub bytes: &'static [u8],
}

pub const ICONS: &[Icon] = &[
    Icon {
        path: "/icons/printer-48.png",
        size: 48,
        bytes: include_bytes!("../assets/printer-48.png"),
    },
    Icon {
        path: "/icons/printer-128.png",
        size: 128,
        bytes: include_bytes!("../assets/printer-128.png"),
    },
    Icon {
        path: "/icons/printer-512.png",
        size: 512,
        bytes: include_bytes!("../assets/printer-512.png"),
    },
];

pub fn find(path: &str) -> Option<&'static Icon> {
    ICONS.iter().find(|icon| icon.path == path)
}
