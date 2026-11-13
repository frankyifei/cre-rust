//! Format-independent book layer shared by every format crate.

pub mod images;
pub mod resources;

pub use images::{
    estimated_decoded_from_size, estimated_image_bytes, image_dimensions, is_image_bytes,
};
pub use resources::{
    GeneratedResources, MapResources, ResourceProvider, ResourceSource, generated_resource,
    normalize_key,
};
