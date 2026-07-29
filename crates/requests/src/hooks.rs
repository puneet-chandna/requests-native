//! Python-free ordering state for origin-thread hook dispatch.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HookId(usize);

impl HookId {
    #[must_use]
    pub fn index(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HookValueId(usize);

impl HookValueId {
    #[must_use]
    pub fn new(index: usize) -> Self {
        Self(index)
    }

    #[must_use]
    pub fn index(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HookCall {
    pub hook: HookId,
    pub value: HookValueId,
}

#[derive(Debug)]
pub struct HookRegistry {
    next_hook: usize,
    current_value: HookValueId,
}

impl HookRegistry {
    #[must_use]
    pub fn new(initial_value: HookValueId) -> Self {
        Self {
            next_hook: 0,
            current_value: initial_value,
        }
    }

    #[must_use]
    pub fn next_call(&mut self) -> HookCall {
        let call = HookCall {
            hook: HookId(self.next_hook),
            value: self.current_value,
        };
        self.next_hook += 1;
        call
    }

    pub fn replace(&mut self, value: HookValueId) {
        self.current_value = value;
    }

    #[must_use]
    pub fn current_value(&self) -> HookValueId {
        self.current_value
    }
}

#[cfg(test)]
mod tests {
    use super::{HookRegistry, HookValueId};

    #[test]
    fn registry_orders_calls_and_tracks_replacements() {
        let mut registry = HookRegistry::new(HookValueId::new(2));

        let first = registry.next_call();
        assert_eq!(first.hook.index(), 0);
        assert_eq!(first.value.index(), 2);

        registry.replace(HookValueId::new(7));
        let second = registry.next_call();
        assert_eq!(second.hook.index(), 1);
        assert_eq!(second.value.index(), 7);
        assert_eq!(registry.current_value().index(), 7);
    }
}
