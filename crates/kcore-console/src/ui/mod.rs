mod cols;
mod render;
#[cfg(test)]
mod visual;
pub use render::draw;
#[cfg(test)]
pub use visual::{buffer_contains_on_one_line, buffer_plain, buffer_svg};
