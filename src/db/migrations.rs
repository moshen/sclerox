/// A single schema migration. Version numbers must be dense and ascending.
/// Never remove or reorder entries - only append.
pub struct Migration {
    pub version: u32,
    pub description: &'static str,
    pub sql: &'static str,
}

/// Current target schema version for the primary database.
/// Must equal the highest version in PRIMARY_MIGRATIONS (or 1 if no migrations yet).
pub const PRIMARY_VERSION: u32 = 6;

/// Current target schema version for per-repo databases.
pub const REPO_VERSION: u32 = 1;

/// Migrations for the primary database (~/.ol/ol.db).
///
/// Version 1 is the baseline established by PRIMARY_SCHEMA - it is applied to
/// fresh databases via execute_batch, not through this list.
/// All entries here describe deltas that bring an *existing* database forward.
///
/// Example of adding a new migration:
///   Migration {
///       version: 2,
///       description: "add linkedin_url to people",
///       sql: "ALTER TABLE people ADD COLUMN linkedin_url TEXT;",
///   },
pub const PRIMARY_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 2,
        description: "add todos and investigations tables",
        sql: crate::db::schema::MIGRATION_V2,
    },
    Migration {
        version: 3,
        description: "add memory_people junction table",
        sql: crate::db::schema::MIGRATION_V3,
    },
    Migration {
        version: 4,
        description: "add project_repos junction table",
        sql: crate::db::schema::MIGRATION_V4,
    },
    Migration {
        version: 5,
        description: "add memory status, source, supersession, and reviewed_at columns",
        sql: crate::db::schema::MIGRATION_V5,
    },
    Migration {
        version: 6,
        description: "add investigation_chunks for semantic similarity search",
        sql: crate::db::schema::MIGRATION_V6,
    },
];

/// Migrations for per-repo databases (<repo>/.ol/repo.db).
pub const REPO_MIGRATIONS: &[Migration] = &[
    // (no migrations beyond baseline v1 yet)
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_migrations_are_dense_ascending() {
        // Versions must start at 2 (1 is the baseline) and be contiguous
        for (expected, m) in (2u32..).zip(PRIMARY_MIGRATIONS.iter()) {
            assert_eq!(
                m.version, expected,
                "primary migration gap: expected v{expected}, found v{}",
                m.version
            );
        }
        // PRIMARY_VERSION must equal the last migration version (or 1 if empty)
        let expected_current = if PRIMARY_MIGRATIONS.is_empty() {
            1
        } else {
            PRIMARY_MIGRATIONS.last().unwrap().version
        };
        assert_eq!(
            PRIMARY_VERSION, expected_current,
            "PRIMARY_VERSION ({PRIMARY_VERSION}) must equal the last migration version ({expected_current})"
        );
    }

    #[test]
    fn repo_migrations_are_dense_ascending() {
        for (expected, m) in (2u32..).zip(REPO_MIGRATIONS.iter()) {
            assert_eq!(
                m.version, expected,
                "repo migration gap: expected v{expected}, found v{}",
                m.version
            );
        }
        let expected_current = if REPO_MIGRATIONS.is_empty() {
            1
        } else {
            REPO_MIGRATIONS.last().unwrap().version
        };
        assert_eq!(
            REPO_VERSION, expected_current,
            "REPO_VERSION ({REPO_VERSION}) must equal the last migration version ({expected_current})"
        );
    }
}
