//! KernelSU's modules, as the modules page lists them and turns them on
//! and off. A module starts with the system unless a file named `disable`
//! stands in its folder, so the switch makes or takes that file away.

use super::*;

/// One of KernelSU's modules, as its folder and its module.prop name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KsuModule {
    /// The folder's name in `/data/adb/modules`, which the switch goes by.
    pub id: String,
    /// What its module.prop names it.
    pub name: String,
    /// What its module.prop says it is, as its maker wrote it.
    pub version: Option<String>,
    pub author: Option<String>,
    pub description: Option<String>,
    /// Whether it starts with the system.
    pub enabled: bool,
    /// The module is to be taken away when the system next starts.
    pub removed: bool,
    /// An update waits to replace it when the system next starts.
    pub update: bool,
}

impl App {
    /// The modules, as the runtime last read them, or why it could not.
    pub fn set_modules(&mut self, modules: Result<Vec<KsuModule>, String>) {
        self.state.modules = Some(modules);
        self.refresh();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testing::*;

    fn module(id: &str, enabled: bool) -> KsuModule {
        KsuModule { id: id.into(), name: id.into(), version: None, author: None, description: None, enabled, removed: false, update: false }
    }

    /// The advanced page, then the modules page.
    fn to_modules(app: &mut App, f: &Fonts, i: &DeviceInfo) -> Vec<Effect> {
        let mut fx = tap(app, f, i, Action::Open(Screen::Advanced));
        fx.extend(tap(app, f, i, Action::Open(Screen::Modules)));
        fx
    }

    #[test]
    fn the_modules_are_read_once_the_page_opens_and_a_switch_asks_the_runtime() {
        let (f, i) = (fonts(), info());
        let mut app = App::new(Screen::Home, W, H, PPD);
        app.data_waiting(None);
        assert_eq!(to_modules(&mut app, &f, &i), vec![Effect::ReadModules]);
        app.set_modules(Ok(vec![module("test", true), module("off", false)]));
        // The page read once is not asked to read again it is opened to.
        app.key(Key::Back, 0);
        assert_eq!(tap(&mut app, &f, &i, Action::Open(Screen::Modules)), vec![], "an answer heard is not asked again");
        assert_eq!(tap(&mut app, &f, &i, Action::ToggleModule(0)), vec![Effect::SetModule { id: "test".into(), on: false }]);
        assert_eq!(tap(&mut app, &f, &i, Action::ToggleModule(1)), vec![Effect::SetModule { id: "off".into(), on: true }]);
    }

    #[test]
    fn the_page_asks_for_the_data_partition_before_it_is_open() {
        let (f, i) = (fonts(), info());
        let mut app = App::new(Screen::Home, W, H, PPD);
        assert_eq!(to_modules(&mut app, &f, &i), vec![], "nothing to read yet");
        assert!(app.frame(&f, &i).hits.iter().any(|h| h.action == Action::Open(Screen::Decrypt)));
        app.key(Key::Back, 0);
        // The partition open with the owner's own files still locked is
        // enough: KernelSU's folder is no user's own files.
        app.state.data = Data::Locked { user: 10, keys: Keys::Digits, checking: false };
        assert_eq!(tap(&mut app, &f, &i, Action::Open(Screen::Modules)), vec![Effect::ReadModules]);
    }
}
