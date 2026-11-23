//! One module per page. Each turns the app's state into a view and sends
//! messages back; none of them keeps state or talks to the backend.

pub mod custom;
pub mod history;
pub mod home;
pub mod offline;
pub mod sensors;
pub mod settings;
pub mod system;
pub mod tweaks;
