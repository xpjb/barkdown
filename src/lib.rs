//! 🐶 Barkdown: incremental Markdown, shared syntax colors and Sanscale views.
pub mod markdown;
pub mod math;
pub mod preview;
pub mod syntax;
pub use markdown::{Document, Stream};
pub use preview::{BlockSpacing, Faces, Preview, Scene, Theme};
