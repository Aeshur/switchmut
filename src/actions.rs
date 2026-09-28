use crate::config::Bindings;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Action {
    Next = 0,
    Previous = 1,
    Menu = 5,
}

impl Action {
    pub const ALL: [Self; 3] = [Self::Next, Self::Previous, Self::Menu];

    pub fn label(self) -> &'static str {
        match self {
            Self::Next => "Switch Next",
            Self::Previous => "Switch Previous",
            Self::Menu => "Toggle Menu",
        }
    }

    pub fn menu_label(self) -> &'static str {
        match self {
            Self::Next => "Next",
            Self::Previous => "Previous",
            Self::Menu => "Menu",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Routing {
    Capture(u8),
    Command(Action),
    MenuRelease,
    Unassigned(u8),
    None,
}

/// Assignment owns all button input until it ends. Release only has command
/// meaning for the held radial-menu binding.
pub fn route_button(bindings: &Bindings, button: u8, pressed: bool, capturing: bool) -> Routing {
    if button >= 32 {
        return Routing::None;
    }
    if capturing {
        return if pressed {
            Routing::Capture(button)
        } else {
            Routing::None
        };
    }
    let action = Action::ALL
        .into_iter()
        .find(|action| bindings.get(*action) == Some(button));
    match (action, pressed) {
        (Some(action), true) => Routing::Command(action),
        (Some(Action::Menu), false) => Routing::MenuRelease,
        (None, true) => Routing::Unassigned(button),
        _ => Routing::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignment_suppresses_both_normal_commands_and_menu_release() {
        let bindings = Bindings {
            next: Some(10),
            menu: Some(5),
            ..Bindings::default()
        };
        assert_eq!(
            route_button(&bindings, 10, true, true),
            Routing::Capture(10)
        );
        assert_eq!(route_button(&bindings, 5, false, true), Routing::None);
        assert_eq!(
            route_button(&bindings, 10, true, false),
            Routing::Command(Action::Next)
        );
    }

    #[test]
    fn unassigned_menu_stays_unassigned_and_only_menu_receives_release() {
        let mut bindings = Bindings::default();
        assert_eq!(
            route_button(&bindings, 0, true, false),
            Routing::Unassigned(0)
        );
        bindings.next = Some(3);
        bindings.previous = Some(4);
        bindings.menu = Some(6);
        assert_eq!(route_button(&bindings, 3, false, false), Routing::None);
        assert_eq!(
            route_button(&bindings, 4, true, false),
            Routing::Command(Action::Previous)
        );
        assert_eq!(route_button(&bindings, 4, false, false), Routing::None);
        assert_eq!(
            route_button(&bindings, 6, false, false),
            Routing::MenuRelease
        );
    }

    #[test]
    fn menu_keeps_its_original_stable_command_id() {
        assert_eq!(Action::Menu as u8, 5);
        assert_eq!(Action::ALL, [Action::Next, Action::Previous, Action::Menu]);
    }
}
