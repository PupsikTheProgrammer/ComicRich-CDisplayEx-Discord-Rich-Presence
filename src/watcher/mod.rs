//! Observes CDisplayEx from the outside: its main windows' titles, its open
//! files and its registry state.

pub mod locks;
pub mod registry;
mod window;

pub use window::run;

#[derive(Debug)]
pub enum WatchEvent {
    /// A main window's title changed. While a comic is open it holds the current
    /// image's path inside the archive, e.g. `Inner.cbr\Page 003.jpg`.
    Title { pid: u32, title: String },
    /// The reader switched to this CDisplayEx window.
    Focus(u32),
    /// This CDisplayEx window is gone.
    Closed(u32),
}
