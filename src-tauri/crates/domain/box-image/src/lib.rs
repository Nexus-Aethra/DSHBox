//! dshimage format: parse dockerfile-style build scripts into a manifest,
//! then assemble the manifest plus any embedded extension sources into a
//! content-addressable gzip tarball that the CLI and desktop UI can ship
//! around.
//!
//! The crate is dependency-light on purpose: it knows nothing about DSH
//! containers, only about how to describe a list of extensions. The host
//! (`desktop/app/image.rs`) is responsible for materializing a script into
//! a real container, leaning on `box_extensions` for repository hits,
//! download/copy helpers, and digest computation.

pub mod script;
pub mod manifest;
pub mod archive;
pub mod error;

pub use archive::{read_dshimage, write_dshimage, ImageArchive};
pub use error::ImageError;
pub use manifest::{
    compile_manifest, parse_manifest, serialize_manifest, AddSource, ImageManifest,
    ResolvedAdd, TemplateBase,
};
pub use script::{
    parse_script, parse_source_token, AddKind, ImageOp, ImageScript, ParsedSource,
};
