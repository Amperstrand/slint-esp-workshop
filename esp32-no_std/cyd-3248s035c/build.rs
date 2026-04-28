fn main() {
    slint_build::compile_with_config(
        "ui/dashboard.slint",
        slint_build::CompilerConfiguration::new()
            .with_style("fluent-dark".to_string())
            .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer),
    )
    .expect("Slint build failed");
}
