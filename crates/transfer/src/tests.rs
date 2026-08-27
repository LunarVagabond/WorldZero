use std::sync::Arc;

use character::{AttributeSchema, BoundRealmLiveness, CharacterStore};
use common::config::PostgresConfig;
use common::id::{AccountId, CharacterId};
use common::pool::{PoolOptions, postgres_pool};
use realm_directory::{OpenOrBound, RealmStore};
use sqlx::PgPool;

use crate::audit::{TransferAuditLog, TransferOutcome};
use crate::execute::{TransferExecutor, TransferRequest};
use crate::gate::{PurchaseVerifier, TransferGate, TransferGateStore};

// Real Postgres — set WZ_POSTGRES_* and run with `-- --ignored`.

async fn pool() -> PgPool {
    let pg_config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
    postgres_pool(&pg_config, PoolOptions::default())
        .await
        .unwrap()
}

fn schema(stats_yaml: &str) -> AttributeSchema {
    AttributeSchema::from_yaml(stats_yaml).unwrap()
}

fn source_schema() -> AttributeSchema {
    schema(
        r#"
schema_version: 1
stats:
  - key: hp
    type: int
    default: 100
    min: 0
    max: 100
  - key: source_only_stat
    type: int
    default: 5
"#,
    )
}

fn destination_schema() -> AttributeSchema {
    schema(
        r#"
schema_version: 1
stats:
  - key: hp
    type: int
    default: 100
    min: 0
    max: 100
  - key: mana
    type: int
    default: 50
"#,
    )
}

async fn create_account(pool: &PgPool) -> AccountId {
    let account_id = AccountId::new();
    sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
        .bind(account_id.as_uuid())
        .bind(format!("transfer-test-{account_id}"))
        .execute(pool)
        .await
        .unwrap();
    account_id
}

async fn executor(pool: PgPool) -> TransferExecutor {
    TransferExecutor::new(
        pool.clone(),
        RealmStore::new(pool.clone()),
        TransferGateStore::new(pool.clone()),
        TransferAuditLog::new(pool),
    )
}

#[tokio::test]
#[ignore]
async fn a_successful_transfer_moves_the_realm_and_migrates_stats() {
    let pool = pool().await;
    let realms = RealmStore::new(pool.clone());
    let source_realm_id = realms.create("Source", OpenOrBound::Bound).await.unwrap();
    let destination_realm_id = realms
        .create("Destination", OpenOrBound::Bound)
        .await
        .unwrap();

    let account_id = create_account(&pool).await;
    let store = CharacterStore::new(pool.clone(), source_schema(), Default::default());
    let character_id = store
        .create(account_id, "Aria", source_realm_id, "greenwood-forest")
        .await
        .unwrap();
    store.set_stat(character_id, "hp", 42).await.unwrap();
    store
        .set_stat(character_id, "source_only_stat", 7)
        .await
        .unwrap();

    let destination_schema = destination_schema();
    executor(pool.clone())
        .await
        .transfer(TransferRequest {
            character_id,
            destination_realm_id,
            destination_schema: &destination_schema,
            initiated_by: account_id,
        })
        .await
        .unwrap();

    let realm_id: uuid::Uuid = sqlx::query_scalar("SELECT realm_id FROM characters WHERE id = $1")
        .bind(character_id.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(realm_id, destination_realm_id.as_uuid());

    let stats: serde_json::Value = sqlx::query_scalar("SELECT stats FROM characters WHERE id = $1")
        .bind(character_id.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    // hp carried over (declared on both sides); source_only_stat dropped
    // (destination never declared it); mana filled with the
    // destination's default (character never had it at all).
    assert_eq!(stats["hp"], 42);
    assert_eq!(stats["mana"], 50);
    assert!(stats.get("source_only_stat").is_none(), "{stats:?}");
}

#[tokio::test]
#[ignore]
async fn transferring_an_open_realm_character_is_rejected() {
    let pool = pool().await;
    let realms = RealmStore::new(pool.clone());
    let open_realm_id = realms.create("Open Home", OpenOrBound::Open).await.unwrap();
    let destination_realm_id = realms
        .create("Destination", OpenOrBound::Bound)
        .await
        .unwrap();

    let account_id = create_account(&pool).await;
    let store = CharacterStore::new(pool.clone(), source_schema(), Default::default());
    let character_id = store
        .create(account_id, "Aria", open_realm_id, "greenwood-forest")
        .await
        .unwrap();

    let destination_schema = destination_schema();
    let err = executor(pool.clone())
        .await
        .transfer(TransferRequest {
            character_id,
            destination_realm_id,
            destination_schema: &destination_schema,
            initiated_by: account_id,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("open realm"), "{err}");
}

/// The "failed transfer leaves the character usable on the source realm"
/// acceptance criterion — a nonexistent destination realm is rejected
/// before the transaction ever commits, and the character's row is
/// completely untouched afterward.
#[tokio::test]
#[ignore]
async fn a_failed_transfer_leaves_the_character_unchanged_on_the_source_realm() {
    let pool = pool().await;
    let realms = RealmStore::new(pool.clone());
    let source_realm_id = realms.create("Source", OpenOrBound::Bound).await.unwrap();

    let account_id = create_account(&pool).await;
    let store = CharacterStore::new(pool.clone(), source_schema(), Default::default());
    let character_id = store
        .create(account_id, "Aria", source_realm_id, "greenwood-forest")
        .await
        .unwrap();
    store.set_stat(character_id, "hp", 42).await.unwrap();

    let destination_schema = destination_schema();
    let err = executor(pool.clone())
        .await
        .transfer(TransferRequest {
            character_id,
            destination_realm_id: common::id::RealmId::new(),
            destination_schema: &destination_schema,
            initiated_by: account_id,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no realm with id"), "{err}");

    let realm_id: uuid::Uuid = sqlx::query_scalar("SELECT realm_id FROM characters WHERE id = $1")
        .bind(character_id.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(realm_id, source_realm_id.as_uuid());

    let hp = store.get_stat(character_id, "hp").await.unwrap();
    assert_eq!(hp, 42);
}

/// #169: closing the "known gap" — a bound-realm connection that's
/// registered itself live (`character::BoundRealmLiveness::join`, what
/// `server::session::handle_session` calls on join) blocks a transfer,
/// and releasing it (`leave`, what disconnect calls) unblocks one.
#[tokio::test]
#[ignore]
async fn a_transfer_is_rejected_while_the_character_has_an_active_bound_realm_connection() {
    let pool = pool().await;
    let (_realms, source_realm_id, destination_realm_id) = source_and_destination(&pool).await;
    let store = CharacterStore::new(pool.clone(), source_schema(), Default::default());
    let (character_id, account_id) = create_character(&pool, &store, source_realm_id).await;

    let liveness = BoundRealmLiveness::new(pool.clone());
    liveness
        .join(
            character_id,
            source_realm_id,
            std::time::Duration::from_secs(30),
        )
        .await
        .unwrap();

    let destination_schema = destination_schema();
    let err = executor(pool.clone())
        .await
        .transfer(TransferRequest {
            character_id,
            destination_realm_id,
            destination_schema: &destination_schema,
            initiated_by: account_id,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("currently logged in"), "{err}");

    // Untouched: still on the source realm.
    let realm_id: uuid::Uuid = sqlx::query_scalar("SELECT realm_id FROM characters WHERE id = $1")
        .bind(character_id.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(realm_id, source_realm_id.as_uuid());

    // Once the connection ends (`leave`), the same transfer succeeds.
    liveness.leave(character_id).await.unwrap();
    executor(pool.clone())
        .await
        .transfer(TransferRequest {
            character_id,
            destination_realm_id,
            destination_schema: &destination_schema,
            initiated_by: account_id,
        })
        .await
        .unwrap();

    let realm_id: uuid::Uuid = sqlx::query_scalar("SELECT realm_id FROM characters WHERE id = $1")
        .bind(character_id.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(realm_id, destination_realm_id.as_uuid());
}

#[tokio::test]
#[ignore]
async fn transferring_into_an_open_realm_is_rejected() {
    let pool = pool().await;
    let realms = RealmStore::new(pool.clone());
    let source_realm_id = realms.create("Source", OpenOrBound::Bound).await.unwrap();
    let open_destination_id = realms
        .create("Open Destination", OpenOrBound::Open)
        .await
        .unwrap();

    let account_id = create_account(&pool).await;
    let store = CharacterStore::new(pool.clone(), source_schema(), Default::default());
    let character_id = store
        .create(account_id, "Aria", source_realm_id, "greenwood-forest")
        .await
        .unwrap();

    let destination_schema = destination_schema();
    let err = executor(pool.clone())
        .await
        .transfer(TransferRequest {
            character_id,
            destination_realm_id: open_destination_id,
            destination_schema: &destination_schema,
            initiated_by: account_id,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("is open"), "{err}");
}

async fn source_and_destination(
    pool: &PgPool,
) -> (RealmStore, common::id::RealmId, common::id::RealmId) {
    let realms = RealmStore::new(pool.clone());
    let source_realm_id = realms.create("Source", OpenOrBound::Bound).await.unwrap();
    let destination_realm_id = realms
        .create("Destination", OpenOrBound::Bound)
        .await
        .unwrap();
    (realms, source_realm_id, destination_realm_id)
}

async fn create_character(
    pool: &PgPool,
    store: &CharacterStore,
    realm_id: common::id::RealmId,
) -> (CharacterId, AccountId) {
    let account_id = create_account(pool).await;
    let character_id = store
        .create(account_id, "Aria", realm_id, "greenwood-forest")
        .await
        .unwrap();
    (character_id, account_id)
}

#[tokio::test]
#[ignore]
async fn a_ticket_item_gate_is_consumed_on_a_successful_transfer() {
    let pool = pool().await;
    let (_realms, source_realm_id, destination_realm_id) = source_and_destination(&pool).await;
    let gates = TransferGateStore::new(pool.clone());
    gates
        .set(
            source_realm_id,
            destination_realm_id,
            TransferGate::TicketItem {
                item_type: "realm_transfer_ticket".to_string(),
            },
        )
        .await
        .unwrap();

    let store = CharacterStore::new(pool.clone(), source_schema(), Default::default());
    let (character_id, account_id) = create_character(&pool, &store, source_realm_id).await;
    store
        .grant_item(character_id, "realm_transfer_ticket", 1)
        .await
        .unwrap();

    let destination_schema = destination_schema();
    executor(pool.clone())
        .await
        .transfer(TransferRequest {
            character_id,
            destination_realm_id,
            destination_schema: &destination_schema,
            initiated_by: account_id,
        })
        .await
        .unwrap();

    assert_eq!(
        store
            .item_quantity(character_id, "realm_transfer_ticket")
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
#[ignore]
async fn a_ticket_item_gate_without_the_item_is_rejected_and_nothing_is_consumed() {
    let pool = pool().await;
    let (_realms, source_realm_id, destination_realm_id) = source_and_destination(&pool).await;
    let gates = TransferGateStore::new(pool.clone());
    gates
        .set(
            source_realm_id,
            destination_realm_id,
            TransferGate::TicketItem {
                item_type: "realm_transfer_ticket".to_string(),
            },
        )
        .await
        .unwrap();

    let store = CharacterStore::new(pool.clone(), source_schema(), Default::default());
    let (character_id, account_id) = create_character(&pool, &store, source_realm_id).await;
    // Owns a different item, but not the one the gate requires.
    store
        .grant_item(character_id, "unrelated_item", 3)
        .await
        .unwrap();

    let destination_schema = destination_schema();
    let err = executor(pool.clone())
        .await
        .transfer(TransferRequest {
            character_id,
            destination_realm_id,
            destination_schema: &destination_schema,
            initiated_by: account_id,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("realm_transfer_ticket"), "{err}");

    // Untouched: still on the source realm, unrelated item still owned.
    let realm_id: uuid::Uuid = sqlx::query_scalar("SELECT realm_id FROM characters WHERE id = $1")
        .bind(character_id.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(realm_id, source_realm_id.as_uuid());
    assert_eq!(
        store
            .item_quantity(character_id, "unrelated_item")
            .await
            .unwrap(),
        3
    );
}

#[tokio::test]
#[ignore]
async fn a_purchase_gate_with_no_verifier_configured_denies_by_default() {
    let pool = pool().await;
    let (_realms, source_realm_id, destination_realm_id) = source_and_destination(&pool).await;
    let gates = TransferGateStore::new(pool.clone());
    gates
        .set(
            source_realm_id,
            destination_realm_id,
            TransferGate::Purchase {
                product_id: "realm-transfer-token".to_string(),
            },
        )
        .await
        .unwrap();

    let store = CharacterStore::new(pool.clone(), source_schema(), Default::default());
    let (character_id, account_id) = create_character(&pool, &store, source_realm_id).await;

    let destination_schema = destination_schema();
    let err = executor(pool.clone())
        .await
        .transfer(TransferRequest {
            character_id,
            destination_realm_id,
            destination_schema: &destination_schema,
            initiated_by: account_id,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("verified purchase"), "{err}");
}

struct AlwaysVerifiedPurchase;

#[async_trait::async_trait]
impl PurchaseVerifier for AlwaysVerifiedPurchase {
    async fn verify_purchase(
        &self,
        _character_id: CharacterId,
        _product_id: &str,
    ) -> common::Result<bool> {
        Ok(true)
    }
}

#[tokio::test]
#[ignore]
async fn a_purchase_gate_succeeds_once_a_verifier_confirms_it() {
    let pool = pool().await;
    let (_realms, source_realm_id, destination_realm_id) = source_and_destination(&pool).await;
    let gates = TransferGateStore::new(pool.clone());
    gates
        .set(
            source_realm_id,
            destination_realm_id,
            TransferGate::Purchase {
                product_id: "realm-transfer-token".to_string(),
            },
        )
        .await
        .unwrap();

    let store = CharacterStore::new(pool.clone(), source_schema(), Default::default());
    let (character_id, account_id) = create_character(&pool, &store, source_realm_id).await;

    let destination_schema = destination_schema();
    executor(pool.clone())
        .await
        .with_purchase_verifier(Arc::new(AlwaysVerifiedPurchase))
        .transfer(TransferRequest {
            character_id,
            destination_realm_id,
            destination_schema: &destination_schema,
            initiated_by: account_id,
        })
        .await
        .unwrap();

    let realm_id: uuid::Uuid = sqlx::query_scalar("SELECT realm_id FROM characters WHERE id = $1")
        .bind(character_id.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(realm_id, destination_realm_id.as_uuid());
}

#[tokio::test]
#[ignore]
async fn an_unconfigured_gate_defaults_to_open() {
    let pool = pool().await;
    let (_realms, source_realm_id, destination_realm_id) = source_and_destination(&pool).await;
    // Deliberately no `gates.set(...)` call — no row at all for this pair.

    let store = CharacterStore::new(pool.clone(), source_schema(), Default::default());
    let (character_id, account_id) = create_character(&pool, &store, source_realm_id).await;

    let destination_schema = destination_schema();
    executor(pool.clone())
        .await
        .transfer(TransferRequest {
            character_id,
            destination_realm_id,
            destination_schema: &destination_schema,
            initiated_by: account_id,
        })
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn a_successful_transfer_records_a_success_audit_entry() {
    let pool = pool().await;
    let (_realms, source_realm_id, destination_realm_id) = source_and_destination(&pool).await;
    let store = CharacterStore::new(pool.clone(), source_schema(), Default::default());
    let (character_id, account_id) = create_character(&pool, &store, source_realm_id).await;

    let destination_schema = destination_schema();
    executor(pool.clone())
        .await
        .transfer(TransferRequest {
            character_id,
            destination_realm_id,
            destination_schema: &destination_schema,
            initiated_by: account_id,
        })
        .await
        .unwrap();

    let audit = TransferAuditLog::new(pool);
    let history = audit.history_for_character(character_id).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].character_id, character_id);
    assert_eq!(history[0].source_realm_id, Some(source_realm_id));
    assert_eq!(history[0].destination_realm_id, destination_realm_id);
    assert_eq!(history[0].gate_type.as_deref(), Some("open"));
    assert_eq!(history[0].initiated_by, account_id);
    assert_eq!(history[0].outcome, TransferOutcome::Success);
}

#[tokio::test]
#[ignore]
async fn a_failed_transfer_records_a_failure_audit_entry_with_the_reason() {
    let pool = pool().await;
    let (_realms, source_realm_id, _destination_realm_id) = source_and_destination(&pool).await;
    let store = CharacterStore::new(pool.clone(), source_schema(), Default::default());
    let (character_id, account_id) = create_character(&pool, &store, source_realm_id).await;

    let destination_schema = destination_schema();
    let nonexistent_destination = common::id::RealmId::new();
    executor(pool.clone())
        .await
        .transfer(TransferRequest {
            character_id,
            destination_realm_id: nonexistent_destination,
            destination_schema: &destination_schema,
            initiated_by: account_id,
        })
        .await
        .unwrap_err();

    let audit = TransferAuditLog::new(pool);
    let history = audit.history_for_character(character_id).await.unwrap();
    assert_eq!(history.len(), 1);
    // source_realm_id was already known (the character row loaded fine)
    // even though the attempt failed on the *destination* — a partial,
    // honest record of what was actually determined.
    assert_eq!(history[0].source_realm_id, Some(source_realm_id));
    assert_eq!(history[0].destination_realm_id, nonexistent_destination);
    match &history[0].outcome {
        TransferOutcome::Failed { reason } => {
            assert!(reason.contains("no realm with id"), "{reason}");
        }
        other => panic!("expected a Failed outcome, got {other:?}"),
    }
}

#[tokio::test]
#[ignore]
async fn history_for_character_only_returns_that_characters_own_attempts() {
    let pool = pool().await;
    let (_realms, source_realm_id, destination_realm_id) = source_and_destination(&pool).await;
    let store = CharacterStore::new(pool.clone(), source_schema(), Default::default());
    let (character_a, account_a) = create_character(&pool, &store, source_realm_id).await;
    let (character_b, account_b) = create_character(&pool, &store, source_realm_id).await;

    let destination_schema = destination_schema();
    executor(pool.clone())
        .await
        .transfer(TransferRequest {
            character_id: character_a,
            destination_realm_id,
            destination_schema: &destination_schema,
            initiated_by: account_a,
        })
        .await
        .unwrap();

    let audit = TransferAuditLog::new(pool);
    assert_eq!(
        audit
            .history_for_character(character_a)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        audit
            .history_for_character(character_b)
            .await
            .unwrap()
            .is_empty()
    );
    let _ = account_b;
}
