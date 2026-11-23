//! Images on the host: references, registry pulls and the blob store.

#![forbid(unsafe_code)]

pub mod archive;
pub mod b64;
pub mod cancel;
pub mod config;
mod connect;
pub mod jsonfile;
pub mod mirror;
pub mod reference;
pub mod registry;
pub mod store;
pub mod tags;

pub use cancel::Cancel;
pub use config::{Insecure, RegistryConfig};
pub use mirror::Mirror;
pub use reference::Reference;
pub use registry::{
    Client, ConfigSource, Credentials, LooseCredentials, Platform, Progress, PushProgress, set_config_source,
    set_proxy_source,
};
pub use store::{Image, Layer, RunConfig, Store};
