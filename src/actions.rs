use crate::config::Bindings;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Action {
    Next,
    Previous,
    Menu,
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
    None,
}

/// Assignment owns all button input until it ends. Release only has command
/// meaning for the held radial-menu binding.
fn route_button(bindings: &Bindings, button: u8, pressed: bool, capturing: bool) -> Routing {
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
        _ => Routing::None,
    }
}

/// Return the route and updated suppression mask; edges awaiting selection leave the mask
/// unchanged, and capture presses override existing suppression.
pub(crate) fn route_button_edge(
    bindings: &Bindings,
    button: u8,
    pressed: bool,
    capturing: bool,
    awaiting_selection: bool,
    suppress_until_release: u32,
) -> (Routing, u32) {
    if button >= 32 || awaiting_selection {
        return (Routing::None, suppress_until_release);
    }
    let mask = 1u32 << button;
    let suppression = if pressed {
        suppress_until_release
    } else {
        suppress_until_release & !mask
    };
    let routing = route_button(bindings, button, pressed, capturing);
    if matches!(routing, Routing::Capture(_)) {
        (routing, suppression | mask)
    } else if suppression & mask != 0 {
        (Routing::None, suppression)
    } else {
        (routing, suppression)
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
        assert_eq!(route_button(&bindings, 0, true, false), Routing::None);
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
    fn invalid_and_awaiting_selection_edges_preserve_suppression() {
        let bindings = Bindings {
            next: Some(3),
            ..Bindings::default()
        };
        for button in [32, u8::MAX] {
            for pressed in [false, true] {
                assert_eq!(
                    route_button_edge(&bindings, button, pressed, true, false, 8),
                    (Routing::None, 8)
                );
            }
        }
        for pressed in [false, true] {
            for capturing in [false, true] {
                assert_eq!(
                    route_button_edge(&bindings, 3, pressed, capturing, true, 8),
                    (Routing::None, 8)
                );
            }
        }
    }

    #[test]
    fn capture_press_suppresses_and_release_clears_only_its_button() {
        let bindings = Bindings {
            menu: Some(5),
            ..Bindings::default()
        };
        let (routing, suppression) = route_button_edge(&bindings, 5, true, true, false, 8);
        assert_eq!(routing, Routing::Capture(5));
        assert_eq!(suppression, 8 | 32);
        // Capture takes precedence over suppression.
        assert_eq!(
            route_button_edge(&bindings, 5, true, true, false, suppression),
            (Routing::Capture(5), suppression)
        );
        assert_eq!(
            route_button_edge(&bindings, 5, false, true, false, suppression),
            (Routing::None, 8)
        );
    }

    #[test]
    fn suppressed_press_is_ignored_until_release() {
        let bindings = Bindings {
            next: Some(3),
            ..Bindings::default()
        };
        assert_eq!(
            route_button_edge(&bindings, 3, true, false, false, 8 | 32),
            (Routing::None, 8 | 32)
        );
        let (routing, suppression) = route_button_edge(&bindings, 3, false, false, false, 8 | 32);
        assert_eq!((routing, suppression), (Routing::None, 32));
        assert_eq!(
            route_button_edge(&bindings, 3, true, false, false, suppression),
            (Routing::Command(Action::Next), 32)
        );
    }

    #[test]
    fn gated_release_routes_only_the_menu_binding() {
        let bindings = Bindings {
            next: Some(3),
            previous: Some(4),
            menu: Some(5),
        };
        for button in [3, 4, 6] {
            assert_eq!(
                route_button_edge(&bindings, button, false, false, false, 1 << button),
                (Routing::None, 0)
            );
        }
        assert_eq!(
            route_button_edge(&bindings, 5, false, false, false, 32),
            (Routing::MenuRelease, 0)
        );
    }
}
