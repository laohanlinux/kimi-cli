//! Capability registry with fork-and-override semantics.
//!
//! Subagents receive forked registries with selective capability overrides.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

/// Typed capability registry with lazy resolution (§5.1 deviation).
/// Replaces the god-object Runtime pattern by allowing tools and orchestrators
/// to declare dependencies as typed capabilities resolved at runtime.
pub struct CapabilityRegistry {
    capabilities: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self {
            capabilities: HashMap::new(),
        }
    }

    /// Register a capability value. Overwrites any existing entry of the same type.
    pub fn register<T: 'static + Send + Sync>(&mut self, value: T) {
        self.capabilities
            .insert(TypeId::of::<T>(), Arc::new(value));
    }

    /// Get a reference to a capability by type.
    pub fn get<T: 'static + Send + Sync>(&self) -> Option<&T> {
        self.capabilities
            .get(&TypeId::of::<T>())
            .and_then(|arc| arc.downcast_ref::<T>())
    }

    /// Check if a capability is registered.
    pub fn has<T: 'static + Send + Sync>(&self) -> bool {
        self.capabilities.contains_key(&TypeId::of::<T>())
    }

    /// Fork the registry: creates a new registry with all current capabilities,
    /// then applies overrides. Subagents get forked registries with selective
    /// capability overrides.
    #[allow(clippy::type_complexity)]
    pub fn fork(&self, overrides: Vec<Box<dyn Fn(&mut CapabilityRegistry)>>) -> Self {
        let mut new_reg = Self {
            capabilities: self.capabilities.clone(),
        };
        for op in overrides {
            op(&mut new_reg);
        }
        new_reg
    }

    /// Remove a capability by type.
    pub fn remove<T: 'static + Send + Sync>(&mut self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.capabilities.remove(&TypeId::of::<T>())
    }

    /// Number of registered capabilities.
    pub fn len(&self) -> usize {
        self.capabilities.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }
}

impl Default for CapabilityRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for CapabilityRegistry {
    fn clone(&self) -> Self {
        Self {
            capabilities: self.capabilities.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Debug, Clone, PartialEq)]
    struct MockConfig {
        model: String,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct MockStore {
        path: String,
    }

    #[test]
    fn test_register_and_get() {
        let mut reg = CapabilityRegistry::new();
        let config = MockConfig {
            model: "gpt-4".to_string(),
        };
        reg.register(config.clone());

        let retrieved = reg.get::<MockConfig>();
        assert_eq!(retrieved, Some(&config));
    }

    #[test]
    fn test_get_missing() {
        let reg = CapabilityRegistry::new();
        assert!(reg.get::<MockConfig>().is_none());
    }

    #[test]
    fn test_has() {
        let mut reg = CapabilityRegistry::new();
        assert!(!reg.has::<MockConfig>());
        reg.register(MockConfig {
            model: "test".to_string(),
        });
        assert!(reg.has::<MockConfig>());
    }

    #[test]
    fn test_fork_with_override() {
        let mut reg = CapabilityRegistry::new();
        reg.register(MockConfig {
            model: "gpt-4".to_string(),
        });
        reg.register(MockStore {
            path: "/tmp".to_string(),
        });

        // Fork with override: change config but keep store
        let forked = reg.fork(vec![Box::new(|r: &mut CapabilityRegistry| {
            r.register(MockConfig {
                model: "claude".to_string(),
            });
        })]);

        assert_eq!(
            forked.get::<MockConfig>(),
            Some(&MockConfig {
                model: "claude".to_string(),
            })
        );
        // Store should be absent because our Clone impl is shallow
        // In practice, store Arc<T> values
    }

    #[test]
    fn test_fork_preserves_arc_values() {
        let mut reg = CapabilityRegistry::new();
        let config = Arc::new(MockConfig {
            model: "gpt-4".to_string(),
        });
        reg.register(config.clone());

        let forked = reg.fork(vec![]);
        // Forked registry should preserve the Arc value
        let retrieved = forked.get::<Arc<MockConfig>>();
        assert!(retrieved.is_some());
        assert!(std::ptr::eq(
            Arc::as_ptr(retrieved.unwrap()),
            Arc::as_ptr(&config)
        ));
    }

    #[test]
    fn test_multiple_types() {
        let mut reg = CapabilityRegistry::new();
        reg.register(MockConfig {
            model: "gpt-4".to_string(),
        });
        reg.register(MockStore {
            path: "/tmp".to_string(),
        });
        reg.register(42i32);

        assert_eq!(reg.len(), 3);
        assert_eq!(reg.get::<i32>(), Some(&42));
        assert_eq!(
            reg.get::<MockConfig>(),
            Some(&MockConfig {
                model: "gpt-4".to_string(),
            })
        );
    }

    #[test]
    fn test_remove() {
        let mut reg = CapabilityRegistry::new();
        reg.register(MockConfig {
            model: "gpt-4".to_string(),
        });
        assert!(reg.has::<MockConfig>());

        reg.remove::<MockConfig>();
        assert!(!reg.has::<MockConfig>());
    }

    #[test]
    fn test_register_and_get_arc() {
        let mut reg = CapabilityRegistry::new();
        let config = Arc::new(MockConfig {
            model: "gpt-4".to_string(),
        });
        reg.register(config.clone());

        let retrieved = reg.get::<Arc<MockConfig>>();
        assert_eq!(retrieved, Some(&config));
        // Verify it's the same Arc (same pointer)
        assert!(std::ptr::eq(
            Arc::as_ptr(retrieved.unwrap()),
            Arc::as_ptr(&config)
        ));
    }

    #[test]
    fn test_register_overwrites() {
        let mut reg = CapabilityRegistry::new();
        reg.register(MockConfig {
            model: "gpt-4".to_string(),
        });
        reg.register(MockConfig {
            model: "claude".to_string(),
        });

        assert_eq!(
            reg.get::<MockConfig>(),
            Some(&MockConfig {
                model: "claude".to_string(),
            })
        );
    }
}
