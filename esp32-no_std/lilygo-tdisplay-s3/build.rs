// Slint build configuration for T-Display S3.
//
// We use EmbedForSoftwareRenderer (NOT SDF) for best font quality on the
// 170px-tall ST7789 display. SDF was tested and reverted — see
// ui/mainwindow.slint for details.
//
// Set SLINT_FONT_SIZES env var at build time to pre-render specific pixel
// sizes, avoiding blurry runtime scaling:
//   SLINT_FONT_SIZES=10,12,14,16,18,20,22 cargo build --release
fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer);
    slint_build::compile_with_config("ui/mainwindow.slint", config).unwrap();
}
