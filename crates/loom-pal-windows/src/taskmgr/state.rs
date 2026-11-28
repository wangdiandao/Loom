//! Durable WAL journal state machine for Windows IFEO Task Manager replacement.
//!
//! Separated from Win32 registry calls behind the [`Store`] trait so crash recovery,
//! conflict refusal, external revocation, and rollback logic are deterministically
//! unit-tested in memory without touching `HKLM`.

use loom_types::TaskManagerReplacementState;
use serde::{Deserialize, Serialize};

/// Raw Windows registry value captured in the recovery journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegValue {
    /// Win32 registry value type (`REG_SZ = 1`, `REG_EXPAND_SZ = 2`, etc.).
    pub kind: u32,
    /// Raw little-endian bytes stored in the registry (including UTF-16 NUL for strings).
    pub bytes: Vec<u8>,
}

impl RegValue {
    /// Encodes a Rust string as a NUL-terminated UTF-16LE `REG_SZ` value.
    pub fn reg_sz(text: &str) -> Self {
        Self {
            kind: 1, // REG_SZ
            bytes: text
                .encode_utf16()
                .chain(Some(0))
                .flat_map(u16::to_le_bytes)
                .collect(),
        }
    }

    /// Decodes a `REG_SZ` (`1`) or `REG_EXPAND_SZ` (`2`) value into text without trailing NULs.
    pub fn as_text(&self) -> Option<String> {
        let (pairs, []) = self.bytes.as_chunks::<2>() else {
            return None;
        };
        if !matches!(self.kind, 1 | 2) {
            return None;
        }
        let units: Vec<u16> = pairs.iter().map(|&pair| u16::from_le_bytes(pair)).collect();
        Some(
            String::from_utf16_lossy(&units)
                .trim_end_matches('\0')
                .to_owned(),
        )
    }
}

/// Read-only observation of `IFEO\taskmgr.exe`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    /// Existing `Debugger` value, if present (including an empty string).
    pub debugger: Option<RegValue>,
    /// `true` if `UseFilter` or any subkey exists under `taskmgr.exe`.
    pub filtered: bool,
}

/// Durable journal commit phase (`Prepared -> Active -> Restoring -> Restored`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    /// Journal persisted prior to writing `HKLM\...\IFEO\taskmgr.exe`.
    Prepared,
    /// Registry write verified and committed.
    Active,
    /// Restore started; registry rollback in progress.
    Restoring,
    /// Original registry state verified and restored.
    Restored,
}

/// Persisted recovery journal (`taskmgr-recovery.toml`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    /// Journal format version (`1`).
    pub version: u32,
    /// Windows account SID that authorized the replacement.
    pub owner_sid: String,
    /// Canonical executable path registered in `replacement`.
    pub executable: String,
    /// Observed state before Loom wrote `Debugger`.
    pub before: Observation,
    /// Exact `Debugger` value written by Loom.
    pub replacement: RegValue,
    /// Current WAL commit phase.
    pub phase: Phase,
}

/// Abstract registry + journal backing store for deterministic testing.
pub trait Store {
    /// Reads the live IFEO observation for `taskmgr.exe`.
    fn observe(&mut self) -> Result<Observation, String>;
    /// Sets or deletes (`None`) the IFEO `Debugger` value and flushes the key.
    fn write_debugger(&mut self, value: Option<&RegValue>) -> Result<(), String>;
    /// Atomically persists the recovery journal to disk.
    fn save(&mut self, journal: &Journal) -> Result<(), String>;
}

/// Classifies the current replacement state without mutating registry or journal.
pub fn classify_state(
    journal: Option<&Journal>,
    live: &Observation,
) -> TaskManagerReplacementState {
    match journal {
        Some(j) => {
            if !live.filtered && live.debugger.as_ref() == Some(&j.replacement) {
                if j.phase == Phase::Active {
                    TaskManagerReplacementState::Enabled
                } else {
                    TaskManagerReplacementState::RecoveryNeeded
                }
            } else if *live != j.before {
                TaskManagerReplacementState::Conflict
            } else if j.phase == Phase::Active {
                TaskManagerReplacementState::Revoked
            } else if matches!(j.phase, Phase::Prepared | Phase::Restoring) {
                TaskManagerReplacementState::RecoveryNeeded
            } else {
                TaskManagerReplacementState::Disabled
            }
        }
        None => {
            if live.debugger.is_some() || live.filtered {
                TaskManagerReplacementState::Conflict
            } else {
                TaskManagerReplacementState::Disabled
            }
        }
    }
}

/// Enables IFEO replacement through a durable `Prepared -> write -> verify -> Active` sequence.
///
/// Refuses to overwrite any existing `Debugger` (even empty), `UseFilter`, or IFEO subkey.
/// Rolls back cleanly if writing or committing fails.
pub fn enable(
    store: &mut impl Store,
    owner_sid: String,
    executable: String,
    replacement: RegValue,
) -> Result<Journal, String> {
    let before = store.observe()?;
    if before.debugger.is_some() || before.filtered {
        return Err(
            "taskmgr.exe already has an IFEO Debugger, UseFilter, or subkey configured".into(),
        );
    }

    let mut journal = Journal {
        version: 1,
        owner_sid,
        executable,
        before,
        replacement,
        phase: Phase::Prepared,
    };
    store.save(&journal)?;

    // Re-check before writing to catch concurrent modifications.
    if store.observe()? != journal.before {
        return Err("taskmgr.exe IFEO configuration changed before enable completed".into());
    }

    if let Err(error) = store.write_debugger(Some(&journal.replacement)) {
        return match restore(store, &mut journal) {
            Ok(()) => Err(error),
            Err(recovery) => Err(format!("{error}; rollback also incomplete: {recovery}")),
        };
    }

    let actual = store.observe()?;
    if actual.filtered || actual.debugger.as_ref() != Some(&journal.replacement) {
        return Err("taskmgr.exe IFEO value changed immediately after write".into());
    }

    journal.phase = Phase::Active;
    if let Err(error) = store.save(&journal) {
        let recovery = restore(store, &mut journal)
            .err()
            .unwrap_or_else(|| "original Task Manager state restored".into());
        return Err(format!(
            "failed to commit active journal ({error}); {recovery}"
        ));
    }

    Ok(journal)
}

/// Restores the original IFEO state recorded in `journal` (idempotent).
///
/// Refuses to delete or overwrite a `Debugger` value modified by a third-party program.
pub fn restore(store: &mut impl Store, journal: &mut Journal) -> Result<(), String> {
    let actual = store.observe()?;
    if actual == journal.before {
        journal.phase = Phase::Restored;
        return store.save(journal);
    }
    if actual.filtered || actual.debugger.as_ref() != Some(&journal.replacement) {
        return Err(
            "taskmgr.exe IFEO configuration was modified externally; refusing to overwrite".into(),
        );
    }

    journal.phase = Phase::Restoring;
    store.save(journal)?;

    if store.observe()? != actual {
        return Err("taskmgr.exe IFEO configuration changed before restore completed".into());
    }

    store.write_debugger(journal.before.debugger.as_ref())?;
    if store.observe()? != journal.before {
        return Err("taskmgr.exe IFEO verification failed after restore".into());
    }

    journal.phase = Phase::Restored;
    store.save(journal)
}

/// Reconciles a previous journal cleanly before re-enabling or uninstalling.
///
/// After a completed `Restored` phase, any subsequently installed third-party `Debugger`
/// belongs to that tool and is left untouched.
pub fn settle(store: &mut impl Store, journal: &mut Journal) -> Result<(), String> {
    if journal.phase == Phase::Restored
        && store.observe()?.debugger.as_ref() != Some(&journal.replacement)
    {
        return Ok(());
    }
    restore(store, journal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MemoryStore {
        live: Observation,
        disk: Option<Journal>,
        writes: usize,
        fail_save: Option<Phase>,
        fail_write: bool,
        observed: usize,
        change_on: Option<usize>,
    }

    impl Store for MemoryStore {
        fn observe(&mut self) -> Result<Observation, String> {
            self.observed += 1;
            if self.change_on == Some(self.observed) {
                self.live.debugger = Some(RegValue::reg_sz("other.exe"));
            }
            Ok(self.live.clone())
        }

        fn write_debugger(&mut self, value: Option<&RegValue>) -> Result<(), String> {
            assert!(
                self.disk.is_some(),
                "journal must be durable before any registry write"
            );
            if self.fail_write {
                return Err("registry write failure".into());
            }
            self.live.debugger = value.cloned();
            self.writes += 1;
            Ok(())
        }

        fn save(&mut self, journal: &Journal) -> Result<(), String> {
            if self.fail_save == Some(journal.phase) {
                return Err("disk failure".into());
            }
            self.disk = Some(journal.clone());
            Ok(())
        }
    }

    fn activate(store: &mut MemoryStore) -> Result<Journal, String> {
        enable(
            store,
            "S-1-5-21-1001".into(),
            r"C:\Program Files\Loom\loom-app.exe".into(),
            RegValue::reg_sz(r#""C:\Program Files\Loom\loom-app.exe" --task-manager"#),
        )
    }

    #[test]
    fn enable_and_restore_are_durable_and_idempotent() {
        let mut store = MemoryStore::default();
        let mut journal = activate(&mut store).expect("enable replacement");
        assert_eq!(
            classify_state(Some(&journal), &store.live),
            TaskManagerReplacementState::Enabled
        );
        assert_eq!(store.disk.as_ref().unwrap().phase, Phase::Active);

        restore(&mut store, &mut journal).expect("first restore");
        restore(&mut store, &mut journal).expect("second restore is idempotent");
        assert_eq!(store.live, Observation::default());
        assert_eq!(store.writes, 2);
        assert_eq!(
            classify_state(Some(&journal), &store.live),
            TaskManagerReplacementState::Disabled
        );
    }

    #[test]
    fn refuses_foreign_values_including_empty_and_filtered_keys() {
        for live in [
            Observation {
                debugger: Some(RegValue::reg_sz("")),
                filtered: false,
            },
            Observation {
                debugger: Some(RegValue::reg_sz("procexp64.exe")),
                filtered: false,
            },
            Observation {
                debugger: None,
                filtered: true,
            },
        ] {
            let mut store = MemoryStore {
                live: live.clone(),
                ..Default::default()
            };
            assert_eq!(
                classify_state(None, &store.live),
                TaskManagerReplacementState::Conflict
            );
            assert!(activate(&mut store).is_err());
            assert_eq!(store.live, live);
            assert!(store.disk.is_none());
        }
    }

    #[test]
    fn backup_failure_never_touches_registry_and_commit_failure_rolls_back() {
        for phase in [Phase::Prepared, Phase::Active] {
            let mut store = MemoryStore {
                fail_save: Some(phase),
                ..Default::default()
            };
            assert!(activate(&mut store).is_err());
            assert_eq!(store.live, Observation::default());
            if phase == Phase::Prepared {
                assert_eq!(store.writes, 0);
            }
        }
    }

    #[test]
    fn interrupted_phases_classify_and_recover_cleanly() {
        for phase in [Phase::Prepared, Phase::Active, Phase::Restoring] {
            let mut store = MemoryStore::default();
            let mut journal = activate(&mut store).unwrap();
            journal.phase = phase;
            store.save(&journal).unwrap();
            if phase != Phase::Active {
                assert_eq!(
                    classify_state(Some(&journal), &store.live),
                    TaskManagerReplacementState::RecoveryNeeded
                );
            }
            let mut recovered = store.disk.clone().unwrap();
            restore(&mut store, &mut recovered).unwrap();
            assert_eq!(store.live, Observation::default());
            assert_eq!(recovered.phase, Phase::Restored);
        }
    }

    #[test]
    fn external_removal_classifies_as_revoked_and_foreign_value_survives_restore() {
        let mut store = MemoryStore::default();
        let mut journal = activate(&mut store).unwrap();

        // Externally removed Debugger while journal is still Active -> Revoked.
        store.live = Observation::default();
        assert_eq!(
            classify_state(Some(&journal), &store.live),
            TaskManagerReplacementState::Revoked
        );

        // Externally replaced by another tool -> Conflict; restore must not clobber it.
        store.live.debugger = Some(RegValue::reg_sz("procexp64.exe"));
        let foreign = store.live.clone();
        assert_eq!(
            classify_state(Some(&journal), &store.live),
            TaskManagerReplacementState::Conflict
        );
        assert!(restore(&mut store, &mut journal).is_err());
        assert_eq!(store.live, foreign);

        // Race right before registry write must abort with zero writes.
        let mut racing = MemoryStore {
            change_on: Some(2),
            ..Default::default()
        };
        assert!(activate(&mut racing).is_err());
        assert_eq!(racing.writes, 0);
    }

    #[test]
    fn finished_restore_preserves_later_third_party_values() {
        let mut store = MemoryStore::default();
        let mut journal = activate(&mut store).unwrap();
        restore(&mut store, &mut journal).unwrap();

        let third_party = Observation {
            debugger: Some(RegValue::reg_sz("procexp64.exe")),
            filtered: false,
        };
        store.live = third_party.clone();
        let writes_before = store.writes;
        settle(&mut store, &mut journal).unwrap();
        assert_eq!(store.live, third_party);
        assert_eq!(store.writes, writes_before);
    }
}
