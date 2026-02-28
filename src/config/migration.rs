//! Config schema version management and forward migrations.
//!
//! # Version history
//!
//! | Version | Description                                                         |
//! |---------|---------------------------------------------------------------------|
//! | 0       | Implicit — no `config_version` field in the TOML file.             |
//! | 1       | Explicit version field added.  Baseline AdaClaw schema.            |
//!
//! When a config without a `config_version` field is loaded it is treated as
//! version 0 and migrated automatically forward to the current version.
//!
//! If the stored version is **newer** than what this binary knows about, an
//! error is returned so the user knows they need to upgrade AdaClaw.
//!
//! # Adding a new migration (future maintainers)
//!
//! 1. Bump `CURRENT_VERSION`.
//! 2. Add a `if cfg.config_version == N { … cfg.config_version = N+1; }` block.
//! 3. Add a unit test for the new step.

use super::schema::Config;

/// The config schema version that this build of AdaClaw understands.
/// **Bump this whenever a breaking change is made to the config schema.**
pub const CURRENT_VERSION: u32 = 1;

/// Migrate `cfg` from its recorded version up to [`CURRENT_VERSION`].
///
/// Returns `(migrated_config, notes)`.  `notes` contains human-readable
/// strings describing what was changed — callers should surface these to the
/// user (e.g. as `tracing::warn!` lines or printed to stdout).
///
/// # Errors
///
/// Returns an error only when `cfg.config_version > CURRENT_VERSION`, meaning
/// the config was written by a **newer** binary and cannot be safely used.
pub fn migrate(mut cfg: Config) -> anyhow::Result<(Config, Vec<String>)> {
    use anyhow::bail;

    if cfg.config_version > CURRENT_VERSION {
        bail!(
            "config_version {} is newer than this build of AdaClaw supports \
             (maximum supported version: {}). \
             Please upgrade AdaClaw to use this config file.",
            cfg.config_version,
            CURRENT_VERSION
        );
    }

    let mut notes: Vec<String> = Vec::new();

    // ── v0 → v1 ───────────────────────────────────────────────────────────────
    // v0 = any config that was written before the version field existed.
    // No structural breaking changes in this step — we just stamp the version
    // number.  Add real field renames / type changes here in the future.
    if cfg.config_version == 0 {
        cfg.config_version = 1;
        notes.push(
            "Your config.toml does not contain a `config_version` field. \
             AdaClaw has treated it as version 0 and migrated it to v1. \
             Add `config_version = 1` at the top of config.toml to silence \
             this notice."
                .to_string(),
        );
    }

    // ── Template: v1 → v2 ─────────────────────────────────────────────────────
    // if cfg.config_version == 1 {
    //     // Example field rename:
    //     //   cfg.security.new_field = cfg.security.old_field;
    //     cfg.config_version = 2;
    //     notes.push("Migrated config from v1 to v2: renamed X to Y.".to_string());
    // }

    debug_assert_eq!(
        cfg.config_version,
        CURRENT_VERSION,
        "migrate() left config at version {}, expected {CURRENT_VERSION}",
        cfg.config_version,
    );

    Ok((cfg, notes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v0_config() -> Config {
        // simulates a config file with no version field (serde default = 0)
        Config { config_version: 0, ..Default::default() }
    }

    // ── Happy-path tests ───────────────────────────────────────────────────────

    #[test]
    fn test_v0_migrates_to_current_version() {
        let (migrated, notes) = migrate(v0_config()).expect("migration should succeed");
        assert_eq!(migrated.config_version, CURRENT_VERSION);
        assert!(!notes.is_empty(), "v0 migration should emit a notice");
        // The notice should mention "version 0" so users understand what happened
        assert!(
            notes[0].contains("version 0"),
            "note should mention 'version 0', got: {}",
            notes[0]
        );
    }

    #[test]
    fn test_current_version_is_noop() {
        let cfg = Config { config_version: CURRENT_VERSION, ..Default::default() };
        let (migrated, notes) = migrate(cfg).expect("migration should succeed");
        assert_eq!(migrated.config_version, CURRENT_VERSION);
        assert!(
            notes.is_empty(),
            "no notes expected for an up-to-date config, got: {:?}",
            notes
        );
    }

    #[test]
    fn test_v0_preserves_other_fields() {
        let mut cfg = v0_config();
        cfg.security.autonomy_level = "full".to_string();

        let (migrated, _) = migrate(cfg).unwrap();
        // Data fields must survive migration unchanged
        assert_eq!(migrated.security.autonomy_level, "full");
    }

    // ── Error path ─────────────────────────────────────────────────────────────

    #[test]
    fn test_future_version_returns_error() {
        let cfg = Config { config_version: CURRENT_VERSION + 1, ..Default::default() };
        let result = migrate(cfg);
        assert!(result.is_err(), "future version should be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("newer than this build"),
            "error message should mention 'newer than this build', got: {msg}"
        );
        assert!(
            msg.contains(&(CURRENT_VERSION + 1).to_string()),
            "error should include the bad version number, got: {msg}"
        );
    }

    #[test]
    fn test_far_future_version_returns_error() {
        let cfg = Config { config_version: 999, ..Default::default() };
        let result = migrate(cfg);
        assert!(result.is_err());
    }

    // ── Idempotency ────────────────────────────────────────────────────────────

    #[test]
    fn test_migration_is_idempotent() {
        // Running migrate() twice on a v0 config should not double-apply anything
        let (first_pass, _) = migrate(v0_config()).unwrap();
        let (second_pass, notes) = migrate(first_pass).unwrap();
        assert_eq!(second_pass.config_version, CURRENT_VERSION);
        assert!(
            notes.is_empty(),
            "second migration pass should be a no-op, got: {:?}",
            notes
        );
    }
}
