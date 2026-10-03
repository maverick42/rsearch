//! Compiles the Slint UI (`ui/app.slint` and its imports) into Rust.
//! The Fluent style is selected explicitly so the application keeps a
//! Windows-consistent look whatever the default for the platform is.

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".to_string());
    slint_build::compile_with_config("ui/app.slint", config).expect("slint compile failed");
}
