use character::{AttributeSchema, CharacterStore};
use common::config::PostgresConfig;
use common::id::AccountId;
use common::pool::{PoolOptions, postgres_pool};
use realm_directory::{OpenOrBound, RealmStore};
use sqlx::PgPool;

use crate::execute::{TransferExecutor, TransferRequest};

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
    TransferExecutor::new(pool.clone(), RealmStore::new(pool))
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
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("is open"), "{err}");
}
