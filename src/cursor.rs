use std::io;

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    input::pointer::CursorIcon,
    utils::{Logical, Point, Transform},
};
use xcursor::{CursorTheme, parser::parse_xcursor};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorSettings {
    theme: String,
    size: u32,
}

impl CursorSettings {
    pub fn environment() -> Self {
        Self {
            theme: std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into()),
            size: std::env::var("XCURSOR_SIZE")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(24),
        }
    }

    pub fn configured(&self, theme: Option<&str>, size: Option<u16>) -> Self {
        Self {
            theme: theme.unwrap_or(&self.theme).into(),
            size: size.map_or(self.size, u32::from),
        }
    }

    pub fn environment_values(&self) -> [(&'static str, String); 2] {
        [
            ("XCURSOR_THEME", self.theme.clone()),
            ("XCURSOR_SIZE", self.size.to_string()),
        ]
    }
}

#[derive(Clone)]
pub struct Cursor {
    pub buffer: MemoryRenderBuffer,
    pub hotspot: Point<i32, Logical>,
}

impl Cursor {
    pub fn load(settings: &CursorSettings, icon: CursorIcon) -> io::Result<Self> {
        let theme = CursorTheme::load(&settings.theme);
        let path = fallback_icons(icon)
            .find_map(|icon| load_icon(&theme, icon))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "theme has no default cursor")
            })?;
        let data = std::fs::read(path)?;
        let images = parse_xcursor(&data)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid XCursor file"))?;
        let image = nearest_image(images, settings.size)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty XCursor file"))?;
        let buffer = MemoryRenderBuffer::from_slice(
            &image.pixels_rgba,
            Fourcc::Argb8888,
            (image.width as i32, image.height as i32),
            1,
            Transform::Normal,
            None,
        );
        Ok(Self {
            buffer,
            hotspot: (image.xhot as i32, image.yhot as i32).into(),
        })
    }
}

fn fallback_icons(icon: CursorIcon) -> impl Iterator<Item = CursorIcon> {
    std::iter::once(icon).chain((icon != CursorIcon::Default).then_some(CursorIcon::Default))
}

fn load_icon(theme: &CursorTheme, icon: CursorIcon) -> Option<std::path::PathBuf> {
    icon_names(icon)
        .into_iter()
        .find_map(|name| theme.load_icon(name))
}

fn icon_names(icon: CursorIcon) -> Vec<&'static str> {
    let mut names = vec![icon.name()];
    names.extend_from_slice(icon.alt_names());
    names
}

fn nearest_image(
    images: Vec<xcursor::parser::Image>,
    requested_size: u32,
) -> Option<xcursor::parser::Image> {
    images
        .into_iter()
        .min_by_key(|image| requested_size.abs_diff(image.size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use xcursor::parser::Image;

    fn image(size: u32) -> Image {
        Image {
            size,
            width: size,
            height: size,
            xhot: 0,
            yhot: 0,
            delay: 0,
            pixels_rgba: vec![],
            pixels_argb: vec![],
        }
    }

    #[test]
    fn selects_nearest_cursor_size() {
        assert_eq!(
            nearest_image(vec![image(16), image(24), image(48)], 30)
                .unwrap()
                .size,
            24
        );
        assert!(nearest_image(Vec::new(), 24).is_none());
    }

    #[test]
    fn configured_settings_fall_back_independently() {
        let inherited = CursorSettings {
            theme: "System".into(),
            size: 20,
        };

        assert_eq!(
            inherited.configured(Some("Breeze"), None),
            CursorSettings {
                theme: "Breeze".into(),
                size: 20,
            }
        );
        assert_eq!(
            inherited.configured(None, Some(32)),
            CursorSettings {
                theme: "System".into(),
                size: 32,
            }
        );
    }

    #[test]
    fn exposes_effective_environment_values() {
        let settings = CursorSettings {
            theme: "Breeze".into(),
            size: 32,
        };

        assert_eq!(
            settings.environment_values(),
            [
                ("XCURSOR_THEME", "Breeze".into()),
                ("XCURSOR_SIZE", "32".into()),
            ]
        );
    }

    #[test]
    fn uses_standard_then_legacy_cursor_names() {
        assert_eq!(
            icon_names(CursorIcon::Pointer),
            ["pointer", "hand2", "hand1", "hand", "pointing_hand"]
        );
        assert_eq!(
            fallback_icons(CursorIcon::Text).collect::<Vec<_>>(),
            [CursorIcon::Text, CursorIcon::Default]
        );
        assert_eq!(
            fallback_icons(CursorIcon::Default).collect::<Vec<_>>(),
            [CursorIcon::Default]
        );
    }
}
