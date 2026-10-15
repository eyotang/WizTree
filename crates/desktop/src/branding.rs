use gpui_kit::{Image, ImageSource};
use std::sync::{Arc, OnceLock};

const LOGO: &[u8] = include_bytes!("../assets/WizTree.png");

pub fn logo() -> ImageSource {
    static IMAGE: OnceLock<Arc<Image>> = OnceLock::new();
    IMAGE
        .get_or_init(|| Arc::new(Image::from_bytes(gpui_kit::ImageFormat::Png, LOGO.to_vec())))
        .clone()
        .into()
}

#[cfg(target_os = "linux")]
pub fn window_icon() -> Arc<image::RgbaImage> {
    Arc::new(
        image::load_from_memory(LOGO)
            .expect("Invalid embedded application icon")
            .into_rgba8(),
    )
}
