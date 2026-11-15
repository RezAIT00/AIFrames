//! Shared egui building blocks for the suite's desktop apps.
//!
//! * [`fonts::FontManager`] – loads Windows system fonts (Calibri, Segoe UI, ...) lazily
//! * [`theme`] – Office-like light theme parameterised by the app's accent colour, colour palette
//! * [`recent`] – recently used files per app

pub mod fonts;
pub mod recent;
pub mod theme;
