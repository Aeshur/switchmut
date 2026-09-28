//! Deterministic client ordering and the v2.7 internal selection cursor.

use crate::clients::{Client, ClientKey};
use std::collections::HashMap;

#[derive(Clone, Debug, Default)]
pub struct Rotation {
    clients: Vec<Client>,
    cursor: usize,
}

impl Rotation {
    /// Reconcile an ordinary discovery pass. Existing windows keep their
    /// relative order and allow setting; new windows append in discovery order.
    /// The legacy monitor started at index zero, then selected the next
    /// eligible client after initial enumeration. If refresh removes the
    /// selected window, it advances once from the old numeric cursor.
    pub fn reconcile(&mut self, discovered: Vec<Client>) {
        let was_empty = self.clients.is_empty();
        let previous = self.current();
        let previous_index = previous
            .and_then(|key| self.clients.iter().position(|client| client.key == key))
            .unwrap_or(self.cursor);
        self.replace_clients(discovered);

        if self.clients.is_empty() {
            self.cursor = 0;
        } else if let Some(previous) = previous {
            if let Some(index) = self
                .clients
                .iter()
                .position(|client| client.key == previous)
            {
                self.cursor = index;
            } else {
                self.cursor = previous_index;
                self.next(false);
            }
        } else if was_empty {
            self.cursor = 0;
            self.next(false);
        } else {
            self.cursor = previous_index;
        }
    }

    /// Reconcile before a next/previous command. Like the source's
    /// `NoActivateNext` refresh, this leaves the numeric cursor untouched so
    /// the caller's following `next` performs exactly one advance.
    pub fn reconcile_for_switch(&mut self, discovered: Vec<Client>) {
        let previous = self.current();
        let previous_index = previous
            .and_then(|key| self.clients.iter().position(|client| client.key == key))
            .unwrap_or(self.cursor);
        self.replace_clients(discovered);

        if self.clients.is_empty() {
            self.cursor = 0;
        } else if let Some(previous) = previous {
            if let Some(index) = self
                .clients
                .iter()
                .position(|client| client.key == previous)
            {
                self.cursor = index;
            } else {
                self.cursor = previous_index;
            }
        } else {
            self.cursor = previous_index;
        }
    }

    /// Advance one allowed client. `previous` selects reverse traversal.
    pub fn next(&mut self, previous: bool) -> Option<ClientKey> {
        let count = self.clients.len();
        if count == 0 {
            return None;
        }

        let mut index = self.cursor;
        for _ in 0..count {
            index = if previous {
                if index == 0 || index > count {
                    count - 1
                } else {
                    index - 1
                }
            } else if index >= count - 1 {
                0
            } else {
                index + 1
            };
            if self.clients[index].allowed {
                self.cursor = index;
                return Some(self.clients[index].key);
            }
        }
        None
    }

    pub fn current(&self) -> Option<ClientKey> {
        self.clients.get(self.cursor).map(|client| client.key)
    }

    /// Move a client to a new list position while keeping the selected client.
    pub fn reorder(&mut self, index: usize, new_index: usize) {
        if index >= self.clients.len() || new_index >= self.clients.len() || index == new_index {
            return;
        }
        let selected = self.current();
        let client = self.clients.remove(index);
        self.clients.insert(new_index, client);
        if let Some(selected) = selected
            && let Some(position) = self
                .clients
                .iter()
                .position(|client| client.key == selected)
        {
            self.cursor = position;
        }
    }

    /// Change whether this client participates in next/previous rotation.
    pub fn allow(&mut self, key: ClientKey, allowed: bool) -> bool {
        let Some(client) = self.clients.iter_mut().find(|client| client.key == key) else {
            return false;
        };
        client.allowed = allowed;
        true
    }

    pub fn clients(&self) -> &[Client] {
        &self.clients
    }

    fn replace_clients(&mut self, discovered: Vec<Client>) {
        let discovery_order: Vec<ClientKey> = discovered.iter().map(|client| client.key).collect();
        let mut discovered_by_key: HashMap<ClientKey, Client> = discovered
            .into_iter()
            .map(|client| (client.key, client))
            .collect();
        let mut merged = Vec::with_capacity(discovered_by_key.len());

        for old in &self.clients {
            if let Some(mut fresh) = discovered_by_key.remove(&old.key) {
                fresh.allowed = old.allowed;
                merged.push(fresh);
            }
        }
        for key in discovery_order {
            if let Some(client) = discovered_by_key.remove(&key) {
                merged.push(client);
            }
        }
        self.clients = merged;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(id: isize, allowed: bool) -> Client {
        Client {
            key: ClientKey {
                hwnd: id,
                pid: id as u32 + 100,
                created: 1,
            },
            class: format!("Window{id}"),
            title: format!("Client {id}"),
            allowed,
        }
    }

    fn keys(rotation: &Rotation) -> Vec<ClientKey> {
        rotation.clients().iter().map(|client| client.key).collect()
    }

    #[test]
    fn empty_rotation_has_no_selection_or_next_client() {
        let mut rotation = Rotation::default();
        rotation.reconcile(Vec::new());
        assert_eq!(rotation.current(), None);
        assert_eq!(rotation.next(false), None);
        assert_eq!(rotation.next(true), None);
    }

    #[test]
    fn initial_enumeration_advances_from_legacy_index_zero() {
        let mut rotation = Rotation::default();
        let first = client(1, true).key;
        let second = client(2, true).key;
        rotation.reconcile(vec![client(1, true), client(2, true)]);
        assert_eq!(rotation.current(), Some(second));
        assert_eq!(rotation.next(false), Some(first));
        assert_eq!(rotation.next(true), Some(second));
    }

    #[test]
    fn single_client_wraps_to_itself_and_all_denied_returns_none() {
        let mut rotation = Rotation::default();
        let only = client(1, true).key;
        rotation.reconcile(vec![client(1, true)]);
        assert_eq!(rotation.current(), Some(only));
        assert_eq!(rotation.next(false), Some(only));
        rotation.allow(only, false);
        assert_eq!(rotation.next(false), None);
    }

    #[test]
    fn skips_denied_clients_in_both_directions() {
        let mut rotation = Rotation::default();
        let one = client(1, true).key;
        let three = client(3, true).key;
        rotation.reconcile(vec![client(1, true), client(2, false), client(3, true)]);
        assert_eq!(rotation.current(), Some(three));
        assert_eq!(rotation.next(false), Some(one));
        assert_eq!(rotation.next(true), Some(three));
    }

    #[test]
    fn refresh_preserves_selection_and_manual_foreground_changes_do_not_move_it() {
        let mut rotation = Rotation::default();
        let one = client(1, true);
        let two = client(2, true);
        rotation.reconcile(vec![one.clone(), two.clone()]);
        assert_eq!(rotation.current(), Some(two.key));

        // Reconcile receives no foreground-window state and preserves the
        // monitor's selected key even if another client was focused manually.
        rotation.reconcile(vec![one, two.clone()]);
        assert_eq!(rotation.current(), Some(two.key));
    }

    #[test]
    fn ordinary_refresh_removal_advances_once_from_the_old_cursor() {
        let mut rotation = Rotation::default();
        let one = client(1, true).key;
        rotation.reconcile(vec![client(1, true), client(2, true), client(3, true)]);
        assert_eq!(rotation.current(), Some(client(2, true).key));

        rotation.reconcile(vec![client(1, true), client(3, true)]);
        assert_eq!(rotation.current(), Some(one));
    }

    #[test]
    fn ordinary_refresh_removing_selected_last_client_wraps_from_raw_cursor() {
        let mut rotation = Rotation::default();
        let first = client(1, true).key;
        let second = client(2, true).key;
        let third = client(3, true).key;
        rotation.reconcile(vec![client(1, true), client(2, true), client(3, true)]);
        assert_eq!(rotation.next(false), Some(third));

        rotation.reconcile(vec![client(1, true), client(2, true)]);
        assert_eq!(rotation.current(), Some(first));
        assert_eq!(rotation.clients()[1].key, second);
    }

    #[test]
    fn switch_refresh_leaves_removal_advance_for_the_switch_command() {
        let mut rotation = Rotation::default();
        let one = client(1, true).key;
        let three = client(3, true).key;
        rotation.reconcile(vec![client(1, true), client(2, true), client(3, true)]);
        rotation.reconcile_for_switch(vec![client(1, true), client(3, true)]);
        assert_eq!(rotation.current(), Some(three));
        assert_eq!(rotation.next(false), Some(one));
    }

    #[test]
    fn switch_refresh_removing_selected_last_client_wraps_from_raw_cursor() {
        let mut rotation = Rotation::default();
        let first = client(1, true).key;
        let second = client(2, true).key;
        let third = client(3, true).key;
        rotation.reconcile(vec![client(1, true), client(2, true), client(3, true)]);
        assert_eq!(rotation.next(false), Some(third));

        rotation.reconcile_for_switch(vec![client(1, true), client(2, true)]);
        assert_eq!(rotation.next(false), Some(first));
        assert_eq!(rotation.clients()[1].key, second);
    }

    #[test]
    fn refresh_keeps_old_order_and_allow_settings_then_appends_new_clients() {
        let mut rotation = Rotation::default();
        let one = client(1, true);
        let two = client(2, true);
        let three = client(3, true);
        rotation.reconcile(vec![one.clone(), two.clone()]);
        rotation.allow(one.key, false);
        rotation.reconcile(vec![three.clone(), two.clone(), one.clone()]);

        assert_eq!(keys(&rotation), vec![one.key, two.key, three.key]);
        assert!(!rotation.clients()[0].allowed);
    }

    #[test]
    fn reorder_keeps_current_identity_and_invalid_indexes_are_ignored() {
        let mut rotation = Rotation::default();
        let one = client(1, true).key;
        let two = client(2, true).key;
        rotation.reconcile(vec![client(1, true), client(2, true)]);
        rotation.reorder(0, 1);
        assert_eq!(rotation.current(), Some(two));
        assert_eq!(keys(&rotation), vec![two, one]);
        rotation.reorder(5, 0);
        assert_eq!(keys(&rotation), vec![two, one]);
    }
}
