pub mod display;
pub mod file;
pub mod html;
pub mod namespace;
pub mod os;
pub mod path;
pub mod time;
pub mod webdav;
pub mod write_conditions;

pub use display::*;
pub use file::*;
pub use html::*;
pub use os::*;
pub use path::*;
pub use time::*;

/// The port to start scanning from if no ports were given.
pub const PORT_SCAN_LOWEST: u16 = 8000;

/// The port to end scanning at if no ports were given.
pub const PORT_SCAN_HIGHEST: u16 = 9999;

/// The app name and version to use with Server response header.
pub const USER_AGENT: &str = concat!("hpip/", env!("CARGO_PKG_VERSION"));

/// Index file extensions to look for if `-i` was not specified and strippable extensions.
pub const INDEX_EXTENSIONS: &[&str] = &["html", "htm", "shtml"];

/// Maximum amount of symlinks to follow in any given path lookup.
pub const MAX_SYMLINKS: usize = 40;
