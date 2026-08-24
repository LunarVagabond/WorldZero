//! Realm registry — CRUD for realm definitions plus which zone-service
//! instance(s) belong to which realm (docs/PROPOSAL.md, "Phased
//! Roadmap," Phase 2: "realm-directory service goes live";
//! docs/specs/Realm_Character_Policy_Spec.md, "The flag").
//!
//! Phase 1 ran a single implicit realm with no registry at all — this is
//! the first point a deployment can define more than one. Not wired into
//! `server`'s combined process yet: that's #50 (dynamic layer
//! assignment) and #51 (open/bound enforcement)'s job, consuming this
//! store for real. `open_or_bound` is carried on every realm record from
//! here on even though enforcement doesn't land until #51 — see this
//! spec's "The flag" section for why storing it now avoids a schema
//! change later.

use common::id::RealmId;
use common::{Error, Result};
use sqlx::{PgPool, Row};

/// Open (OSRS-style: one character reachable from any zone-service
/// instance in the realm group) vs. bound (WoW-style: a character
/// belongs to exactly one realm) — a per-realm-group policy, never a
/// per-deployment global switch (docs/specs/Realm_Character_Policy_Spec.md,
/// "The flag"). Enforcement is #51; this type only carries the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenOrBound {
    Open,
    Bound,
}

impl OpenOrBound {
    fn as_db_str(self) -> &'static str {
        match self {
            OpenOrBound::Open => "open",
            OpenOrBound::Bound => "bound",
        }
    }

    fn from_db_str(value: &str) -> Result<Self> {
        match value {
            "open" => Ok(OpenOrBound::Open),
            "bound" => Ok(OpenOrBound::Bound),
            other => Err(Error::new(
                "realm-directory",
                format!("unrecognized open_or_bound value in storage: {other:?}"),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Realm {
    pub id: RealmId,
    pub name: String,
    pub open_or_bound: OpenOrBound,
}

pub struct RealmStore {
    pool: PgPool,
}

impl RealmStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, name: &str, open_or_bound: OpenOrBound) -> Result<RealmId> {
        let id = RealmId::new();

        sqlx::query("INSERT INTO realms (id, name, open_or_bound) VALUES ($1, $2, $3)")
            .bind(id.as_uuid())
            .bind(name)
            .bind(open_or_bound.as_db_str())
            .execute(&self.pool)
            .await
            .map_err(|e| Error::wrap("realm-directory", "failed to create realm", e))?;

        Ok(id)
    }

    pub async fn get(&self, realm_id: RealmId) -> Result<Option<Realm>> {
        let row = sqlx::query("SELECT id, name, open_or_bound FROM realms WHERE id = $1")
            .bind(realm_id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| Error::wrap("realm-directory", "failed to read realm", e))?;

        row.map(row_to_realm).transpose()
    }

    /// Every realm, ordered by name — a directory listing, not
    /// pagination-sensitive at the scale this is meant for (a
    /// self-hoster's own set of realms, not a public multi-tenant index).
    pub async fn list(&self) -> Result<Vec<Realm>> {
        let rows = sqlx::query("SELECT id, name, open_or_bound FROM realms ORDER BY name")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| Error::wrap("realm-directory", "failed to list realms", e))?;

        rows.into_iter().map(row_to_realm).collect()
    }

    /// Replaces `name`/`open_or_bound` on an existing realm. Rejected if
    /// `realm_id` doesn't name a real realm — never a silent no-op.
    pub async fn update(
        &self,
        realm_id: RealmId,
        name: &str,
        open_or_bound: OpenOrBound,
    ) -> Result<()> {
        let result = sqlx::query(
            "UPDATE realms SET name = $2, open_or_bound = $3, updated_at = now() WHERE id = $1",
        )
        .bind(realm_id.as_uuid())
        .bind(name)
        .bind(open_or_bound.as_db_str())
        .execute(&self.pool)
        .await
        .map_err(|e| Error::wrap("realm-directory", "failed to update realm", e))?;

        if result.rows_affected() == 0 {
            return Err(Error::new(
                "realm-directory",
                format!("no realm with id {realm_id}"),
            ));
        }
        Ok(())
    }

    /// Removes a realm — cascades to `realm_zones` (a deleted realm has
    /// no zones left assigned to it), never leaving an orphaned
    /// zone-to-realm mapping behind.
    pub async fn delete(&self, realm_id: RealmId) -> Result<()> {
        sqlx::query("DELETE FROM realms WHERE id = $1")
            .bind(realm_id.as_uuid())
            .execute(&self.pool)
            .await
            .map_err(|e| Error::wrap("realm-directory", "failed to delete realm", e))?;
        Ok(())
    }

    /// Assigns `zone_id` to `realm_id` — a zone belongs to at most one
    /// realm at a time (`realm_zones.zone_id` is its primary key), so
    /// reassigning an already-assigned zone moves it rather than erroring.
    pub async fn assign_zone(&self, realm_id: RealmId, zone_id: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO realm_zones (zone_id, realm_id) VALUES ($1, $2) \
             ON CONFLICT (zone_id) DO UPDATE SET realm_id = EXCLUDED.realm_id",
        )
        .bind(zone_id)
        .bind(realm_id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(|e| Error::wrap("realm-directory", "failed to assign zone to realm", e))?;
        Ok(())
    }

    /// Unassigns `zone_id` from whichever realm (if any) it currently
    /// belongs to. A harmless no-op if it wasn't assigned — a caller
    /// tearing down a zone shouldn't have to check membership first.
    pub async fn unassign_zone(&self, zone_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM realm_zones WHERE zone_id = $1")
            .bind(zone_id)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::wrap("realm-directory", "failed to unassign zone", e))?;
        Ok(())
    }

    /// Every zone-service instance (by content-manifest zone id)
    /// currently assigned to `realm_id`, in no particular order.
    pub async fn zones_for_realm(&self, realm_id: RealmId) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT zone_id FROM realm_zones WHERE realm_id = $1")
            .bind(realm_id.as_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(|e| Error::wrap("realm-directory", "failed to list realm zones", e))?;

        Ok(rows.into_iter().map(|row| row.get("zone_id")).collect())
    }

    /// Which realm (if any) `zone_id` currently belongs to.
    pub async fn realm_for_zone(&self, zone_id: &str) -> Result<Option<RealmId>> {
        let id: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT realm_id FROM realm_zones WHERE zone_id = $1")
                .bind(zone_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| {
                    Error::wrap("realm-directory", "failed to look up realm for zone", e)
                })?;

        Ok(id.map(RealmId::from_uuid))
    }
}

fn row_to_realm(row: sqlx::postgres::PgRow) -> Result<Realm> {
    let open_or_bound: String = row.get("open_or_bound");
    Ok(Realm {
        id: RealmId::from_uuid(row.get("id")),
        name: row.get("name"),
        open_or_bound: OpenOrBound::from_db_str(&open_or_bound)?,
    })
}

#[cfg(test)]
mod tests {
    use common::config::PostgresConfig;
    use common::pool::{PoolOptions, postgres_pool};

    use super::*;

    // Real Postgres — set WZ_POSTGRES_* and run with `-- --ignored`.
    async fn store() -> RealmStore {
        let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();
        RealmStore::new(pool)
    }

    fn unique_name(label: &str) -> String {
        format!("{label}-{}", RealmId::new())
    }

    #[tokio::test]
    #[ignore]
    async fn create_then_get_round_trips() {
        let store = store().await;
        let name = unique_name("create-then-get");

        let id = store.create(&name, OpenOrBound::Open).await.unwrap();
        let realm = store.get(id).await.unwrap().unwrap();

        assert_eq!(realm.id, id);
        assert_eq!(realm.name, name);
        assert_eq!(realm.open_or_bound, OpenOrBound::Open);
    }

    #[tokio::test]
    #[ignore]
    async fn get_missing_realm_returns_none() {
        let store = store().await;
        assert!(store.get(RealmId::new()).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore]
    async fn list_includes_every_created_realm() {
        let store = store().await;
        let name_a = unique_name("list-a");
        let name_b = unique_name("list-b");

        let id_a = store.create(&name_a, OpenOrBound::Open).await.unwrap();
        let id_b = store.create(&name_b, OpenOrBound::Bound).await.unwrap();

        let listed = store.list().await.unwrap();
        let ids: Vec<_> = listed.iter().map(|r| r.id).collect();
        assert!(ids.contains(&id_a), "{listed:?}");
        assert!(ids.contains(&id_b), "{listed:?}");
    }

    #[tokio::test]
    #[ignore]
    async fn update_changes_name_and_policy() {
        let store = store().await;
        let id = store
            .create(&unique_name("update-me"), OpenOrBound::Open)
            .await
            .unwrap();

        let new_name = unique_name("updated");
        store
            .update(id, &new_name, OpenOrBound::Bound)
            .await
            .unwrap();

        let realm = store.get(id).await.unwrap().unwrap();
        assert_eq!(realm.name, new_name);
        assert_eq!(realm.open_or_bound, OpenOrBound::Bound);
    }

    #[tokio::test]
    #[ignore]
    async fn update_on_a_missing_realm_is_rejected() {
        let store = store().await;
        let err = store
            .update(RealmId::new(), "doesn't exist", OpenOrBound::Open)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no realm with id"), "{err}");
    }

    #[tokio::test]
    #[ignore]
    async fn delete_removes_the_realm() {
        let store = store().await;
        let id = store
            .create(&unique_name("delete-me"), OpenOrBound::Open)
            .await
            .unwrap();

        store.delete(id).await.unwrap();

        assert!(store.get(id).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore]
    async fn deleting_a_realm_cascades_to_its_zone_assignments() {
        let store = store().await;
        let id = store
            .create(&unique_name("delete-cascade"), OpenOrBound::Open)
            .await
            .unwrap();
        let zone_id = format!("zone-{}", RealmId::new());
        store.assign_zone(id, &zone_id).await.unwrap();

        store.delete(id).await.unwrap();

        assert_eq!(store.realm_for_zone(&zone_id).await.unwrap(), None);
    }

    #[tokio::test]
    #[ignore]
    async fn assigning_a_zone_twice_moves_it_rather_than_erroring() {
        let store = store().await;
        let realm_a = store
            .create(&unique_name("assign-a"), OpenOrBound::Open)
            .await
            .unwrap();
        let realm_b = store
            .create(&unique_name("assign-b"), OpenOrBound::Bound)
            .await
            .unwrap();
        let zone_id = format!("zone-{}", RealmId::new());

        store.assign_zone(realm_a, &zone_id).await.unwrap();
        assert_eq!(store.realm_for_zone(&zone_id).await.unwrap(), Some(realm_a));

        store.assign_zone(realm_b, &zone_id).await.unwrap();
        assert_eq!(store.realm_for_zone(&zone_id).await.unwrap(), Some(realm_b));
        // No longer with realm_a.
        assert!(
            !store
                .zones_for_realm(realm_a)
                .await
                .unwrap()
                .contains(&zone_id)
        );
    }

    #[tokio::test]
    #[ignore]
    async fn unassigning_an_unassigned_zone_is_a_harmless_no_op() {
        let store = store().await;
        let zone_id = format!("zone-{}", RealmId::new());
        store.unassign_zone(&zone_id).await.unwrap();
        assert_eq!(store.realm_for_zone(&zone_id).await.unwrap(), None);
    }

    /// Multi-realm scenario per #47's acceptance criteria: two realms,
    /// each with their own distinct zones assigned, and confirms
    /// zone-to-realm lookups never cross between them.
    #[tokio::test]
    #[ignore]
    async fn multiple_realms_each_track_their_own_distinct_zones() {
        let store = store().await;
        let realm_a = store
            .create(&unique_name("multi-a"), OpenOrBound::Open)
            .await
            .unwrap();
        let realm_b = store
            .create(&unique_name("multi-b"), OpenOrBound::Bound)
            .await
            .unwrap();

        let zone_a1 = format!("zone-a1-{}", RealmId::new());
        let zone_a2 = format!("zone-a2-{}", RealmId::new());
        let zone_b1 = format!("zone-b1-{}", RealmId::new());

        store.assign_zone(realm_a, &zone_a1).await.unwrap();
        store.assign_zone(realm_a, &zone_a2).await.unwrap();
        store.assign_zone(realm_b, &zone_b1).await.unwrap();

        let mut zones_a = store.zones_for_realm(realm_a).await.unwrap();
        zones_a.sort();
        let mut expected_a = vec![zone_a1.clone(), zone_a2.clone()];
        expected_a.sort();
        assert_eq!(zones_a, expected_a);

        assert_eq!(
            store.zones_for_realm(realm_b).await.unwrap(),
            vec![zone_b1.clone()]
        );

        assert_eq!(store.realm_for_zone(&zone_a1).await.unwrap(), Some(realm_a));
        assert_eq!(store.realm_for_zone(&zone_b1).await.unwrap(), Some(realm_b));

        // Each realm's own policy is independent and unaffected by the
        // other's zone assignments.
        let realm_a_row = store.get(realm_a).await.unwrap().unwrap();
        let realm_b_row = store.get(realm_b).await.unwrap().unwrap();
        assert_eq!(realm_a_row.open_or_bound, OpenOrBound::Open);
        assert_eq!(realm_b_row.open_or_bound, OpenOrBound::Bound);
    }
}
