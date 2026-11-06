//! Shared display selection and the input barrier between two capture geometries.

use serde::Serialize;

#[derive(Clone, Serialize)]
pub struct Display {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub main: bool,
}

#[derive(Default)]
pub struct Displays {
    available: Vec<Display>,
    selected: Option<u32>,
    pending: Option<u32>,
    generation: u64,
}

impl Displays {
    /// Publish only after a capture pipeline has been built successfully. Completing
    /// even a failed switch requires fresh acknowledgements: capture was rebuilt.
    #[cfg(any(target_os = "macos", test))]
    pub fn publish(&mut self, available: Vec<Display>, selected: u32) {
        self.available = available;
        self.selected = Some(selected);
        if self.pending.take().is_some() {
            self.generation += 1;
        }
    }

    pub fn info(&self) -> Option<serde_json::Value> {
        self.selected.map(|selected| {
            serde_json::json!({
                "available": self.available,
                "selected": selected,
                "generation": self.generation,
                "switching": self.pending.is_some(),
            })
        })
    }

    pub fn request(&mut self, id: u32) -> Result<bool, &'static str> {
        if !self.available.iter().any(|d| d.id == id) {
            return Err("Display is unavailable.");
        }
        if self.pending.is_some() {
            return Err("A display switch is already in progress.");
        }
        if self.selected == Some(id) {
            return Ok(false);
        }
        self.pending = Some(id);
        Ok(true)
    }

    pub fn pending(&self) -> Option<u32> {
        self.pending
    }

    /// Every viewer must have received the new geometry before sending input.
    pub fn allows_input(&self, generation: u64) -> bool {
        self.pending.is_none() && generation == self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn displays() -> Vec<Display> {
        [10, 20]
            .into_iter()
            .map(|id| Display {
                id,
                name: format!("Display {id}"),
                width: 1920,
                height: 1080,
                main: id == 10,
            })
            .collect()
    }

    #[test]
    fn switching_blocks_input_until_the_new_geometry_is_acknowledged() {
        let mut state = Displays::default();
        state.publish(displays(), 10);
        assert!(state.allows_input(0));
        assert_eq!(state.request(20), Ok(true));
        assert!(!state.allows_input(0));
        assert!(
            !state.allows_input(1),
            "an early acknowledgement must not unlock input"
        );
        state.publish(displays(), 20);
        assert!(
            !state.allows_input(0),
            "old coordinates must not move the new display"
        );
        assert!(state.allows_input(1));
        assert_eq!(state.info().unwrap()["selected"], 20);
    }

    #[test]
    fn invalid_noop_and_concurrent_requests_do_not_replace_a_pending_switch() {
        let mut state = Displays::default();
        state.publish(displays(), 10);
        assert!(state.request(999).is_err());
        assert_eq!(state.request(10), Ok(false));
        assert_eq!(state.pending(), None);
        assert_eq!(state.request(20), Ok(true));
        assert!(state.request(10).is_err());
        assert_eq!(state.pending(), Some(20));
    }

    #[test]
    fn failed_switch_can_publish_the_previous_display_without_accepting_stale_input() {
        let mut state = Displays::default();
        state.publish(displays(), 10);
        state.request(20).unwrap();
        let remaining = displays().into_iter().filter(|d| d.id == 10).collect();
        state.publish(remaining, 10);
        assert_eq!(state.pending(), None);
        assert_eq!(state.info().unwrap()["selected"], 10);
        assert!(!state.allows_input(0));
        assert!(state.allows_input(1));
        assert!(state.request(20).is_err());
    }
}
