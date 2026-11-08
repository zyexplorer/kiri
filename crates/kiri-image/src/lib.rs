//! Images on the host: references, registry pulls and the blob store.

#![forbid(unsafe_code)]

pub mod archive;
pub mod b64;
pub mod cancel;
mod connect;
pub mod reference;
pub mod registry;
pub mod store;

pub use cancel::Cancel;
pub use reference::Reference;
pub use reference::{normalize_insecure, set_insecure_source};
pub use registry::{
    Client, Credentials, Platform, Progress, PushProgress, normalize_mirror, set_downloads_source, set_mirrors_source, set_proxy_source,
    set_uploads_source,
};
pub use store::{Image, Layer, RunConfig, Store};
