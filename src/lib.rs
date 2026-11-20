//! TweenFrame: frame generation for FiveM / GTA V.
//!
//! capture (WGC) -> luma pyramid -> bidirectional optical flow -> in-between
//! frames -> click-through overlay, paced to the display.

pub mod capture;
pub mod config;
pub mod engine;
pub mod framegen;
pub mod gpu;
pub mod icon;
pub mod icon_raster;
pub mod log;
pub mod overlay;
pub mod target;
pub mod tray_pin;
pub mod ui;
