#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod indexer;
mod searcher;

use app::FileSearchApp;
use eframe::egui::{self, IconData};
use image::DynamicImage;
use std::sync::Arc;
use std::io::Cursor;
use image::ImageReader;

// 使用 include_bytes! 嵌入字体，确保开发时和打包后都能正确加载
// 路径是相对于 src 目录的相对路径：../fonts/noto.ttf
const FONT_DATA: &[u8] = include_bytes!("../fonts/noto.ttf");
const ICON_ICO: &[u8] = include_bytes!("../assets/favicon.ico");

fn main() -> eframe::Result {
    // 加载图标（如果存在）
    let icon = load_icon();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 700.0])
            .with_title("RustSearch")
            .with_icon(Arc::new(icon)),
        ..Default::default()
    };

    eframe::run_native(
        "RustSearch",
        options,
        Box::new(|cc| {
            // 配置中文字体
            let mut fonts = egui::FontDefinitions::default();

            // 嵌入 Noto Sans 中文字体
            // 使用 include_bytes! 确保字体被编译进二进制文件
            fonts.font_data.insert(
                "noto_sans_cjk".to_owned(),
                egui::FontData::from_static(FONT_DATA),
            );

            // 设置 Proportional 字体优先级
            fonts.families.insert(
                egui::FontFamily::Proportional,
                vec!["noto_sans_cjk".to_owned()],
            );

            // 设置 Monospace 字体
            fonts.families.insert(
                egui::FontFamily::Monospace,
                vec!["noto_sans_cjk".to_owned()],
            );

            cc.egui_ctx.set_fonts(fonts);

            Ok(Box::new(FileSearchApp::new(cc)))
        }),
    )
}

/// 加载图标
/// 使用内嵌的 assets/favicon.ico，确保安装后也能显示正确图标
fn load_icon() -> IconData {
    let reader = ImageReader::new(Cursor::new(ICON_ICO)).with_guessed_format();
    let Ok(reader) = reader else {
        return IconData::default();
    };
    let Ok(img) = reader.decode() else {
        return IconData::default();
    };
    convert_image_to_icon(img)
}

///（兼容）如果你希望运行时从文件替换图标，可在这里恢复文件读取逻辑
#[allow(dead_code)]
fn load_icon_from_file(path: &str) -> IconData {
    if let Ok(reader) = ImageReader::open(path) {
        if let Ok(img) = reader.decode() {
            return convert_image_to_icon(img);
        }
    }
    IconData::default()
}

/// 将 image::DynamicImage 转换为 IconData
fn convert_image_to_icon(img: DynamicImage) -> IconData {
    // 转换为 RGBA8 格式
    let rgba8 = img.to_rgba8();
    let (width, height) = rgba8.dimensions();

    // 获取像素数据
    let rgba = rgba8.into_raw();

    IconData {
        rgba,
        width,
        height,
    }
}
