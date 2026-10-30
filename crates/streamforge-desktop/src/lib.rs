//! StreamForge desktop UI (egui/eframe).
//!
//! Layers, from the bottom:
//! - [`worker::EngineWorker`] runs the engine on its own thread; the UI
//!   thread never blocks on a command.
//! - [`model::Model`] turns UI actions into commands (the same names and
//!   parameters as the JSON-lines protocol) and keeps the state their
//!   responses report. No business logic and no widgets.
//! - [`app::DesktopApp`] draws the model with egui and calls its actions:
//!   the docks of the workspace in [`views`], with the design system in
//!   [`ui`] and what they show computed in [`workspace`].
//!
//! Preview frames take the media path: the engine publishes them into a
//! mailbox that the UI reads each frame.

pub mod app;
pub mod console;
pub mod folder;
pub mod identity;
pub mod model;
pub mod ui;
mod views;
pub mod worker;
pub mod workspace;
