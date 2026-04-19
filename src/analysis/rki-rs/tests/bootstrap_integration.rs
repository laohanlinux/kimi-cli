use std::sync::Arc;

/// Test that Runtime can be created with default config and in-memory store.
#[tokio::test]
async fn test_runtime_bootstrap_in_memory() {
    let hub = rki_rs::wire::RootWireHub::new();
    let approval = Arc::new(rki_rs::approval::ApprovalRuntime::new(hub.clone(), true, vec![]));
    let store = rki_rs::store::Store::open(std::path::Path::new(":memory:")).unwrap();
    let session = rki_rs::session::Session::create(&store, std::env::current_dir().unwrap()).unwrap();

    let config = rki_rs::config::Config {
        max_steps_per_turn: Some(100),
        max_context_size: Some(128_000),
        ..rki_rs::config::Config::default()
    };

    let runtime = rki_rs::runtime::Runtime::new(config, session, approval, hub, store);

    assert_eq!(runtime.environment.os, std::env::consts::OS);
    assert!(!runtime.session.id.is_empty());
    assert!(runtime.session.dir.exists());
}

/// Test ConfigRegistry roundtrip via legacy config.
#[test]
fn test_config_registry_roundtrip() {
    let registry = rki_rs::config_registry::default_registry();
    let config = registry.to_legacy_config();

    assert_eq!(config.default_model, "echo");
    assert_eq!(config.max_steps_per_turn, Some(100));
    assert_eq!(config.max_context_size, Some(128_000));
    assert!(config.supports_vision);
    assert!(!config.ignore_vision_model_hint);
    assert!(config.vision_by_model.is_empty());
}

/// Test that the store can create and resume sessions.
#[test]
fn test_store_session_lifecycle() {
    let store = rki_rs::store::Store::open(std::path::Path::new(":memory:")).unwrap();
    let work_dir = std::env::current_dir().unwrap();

    let session1 = rki_rs::session::Session::create(&store, work_dir.clone()).unwrap();
    assert!(!session1.id.is_empty());

    let session2 = rki_rs::session::Session::discover_latest(&store, work_dir.clone()).unwrap();
    assert_eq!(session1.id, session2.id);
}

/// Test that wire hub broadcasts are received by subscribers.
#[tokio::test]
async fn test_wire_hub_broadcast_roundtrip() {
    let hub = rki_rs::wire::RootWireHub::new();
    let mut rx = hub.subscribe();

    hub.broadcast(rki_rs::wire::WireEvent::TurnBegin {
        user_input: rki_rs::wire::UserInput::text_only("hello"),
    });

    let envelope = rx.recv().await.unwrap();
    assert!(matches!(envelope.event, rki_rs::wire::WireEvent::TurnBegin { .. }));
}
