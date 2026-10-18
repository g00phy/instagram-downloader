//! The window: toolbar, Downloads / LinkGrabber / Settings / Log tabs, status bar.

use super::controller::{ControllerHandle, UiLink};

/// The eframe application.
pub struct IgdlApp {
    _private: (),
}

impl IgdlApp {
    /// `controller` is `None` in tests (the test drives `link` directly).
    pub fn new(link: UiLink, controller: Option<ControllerHandle>) -> Self {
        let _ = (link, controller);
        todo!("view agent")
    }
}

impl eframe::App for IgdlApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, frame: &mut eframe::Frame) {
        let _ = (ui, frame);
        todo!("view agent")
    }
}
