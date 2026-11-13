//! Format-independent book layer shared by every format crate.

pub mod chunk;
pub mod css;
pub mod fonts;
pub mod html;
pub mod images;
pub mod resources;
pub mod ruby;

pub use chunk::*;
pub use fonts::{font_face_declarations, font_faces_in, loadable_font_faces, strip_font_faces};
pub use html::{base64_encode, normalize_path, resolve_href};
pub use images::{
    estimated_decoded_from_size, estimated_image_bytes, image_dimensions, image_sources_in,
    is_image_bytes,
};
pub use resources::{
    GeneratedResources, MapResources, ResourceProvider, ResourceSource, generated_resource,
    normalize_key,
};
pub use ruby::rewrite_ruby;
