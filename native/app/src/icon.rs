use winit::window::Icon;

const ICON_WIDTH: u32 = 128;
const ICON_HEIGHT: u32 = 128;
const ICON_RGBA: &[u8] = include_bytes!("../../assets/app-icon-128.rgba");

/// Application/window icon derived directly from the web favicon.
pub(crate) fn app_icon() -> Option<Icon> {
    Icon::from_rgba(ICON_RGBA.to_vec(), ICON_WIDTH, ICON_HEIGHT).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_icon_has_expected_rgba_size() {
        assert_eq!(ICON_RGBA.len(), (ICON_WIDTH * ICON_HEIGHT * 4) as usize);
        assert!(app_icon().is_some());
    }
}
