use std::io;

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    utils::{Logical, Point, Transform},
};
use xcursor::{CursorTheme, parser::parse_xcursor};

pub struct Cursor {
    pub buffer: MemoryRenderBuffer,
    pub hotspot: Point<i32, Logical>,
}

impl Cursor {
    pub fn load() -> io::Result<Self> {
        let theme_name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
        let requested_size: u32 = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(24);
        let theme = CursorTheme::load(&theme_name);
        let path = theme
            .load_icon("default")
            .or_else(|| theme.load_icon("left_ptr"))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "theme has no default cursor")
            })?;
        let data = std::fs::read(path)?;
        let images = parse_xcursor(&data)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid XCursor file"))?;
        let image = nearest_image(images, requested_size)
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
}
