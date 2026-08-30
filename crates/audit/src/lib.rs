//! An append-only, tamper-evident record of security decisions.
//!
//! Separate from session metadata on purpose. Session rows are updated in
//! place all the time: states change, names change, rows are deleted. An
//! audit trail that lived beside them would inherit that mutability, and a
//! history anyone can quietly edit answers no question worth asking.
//!
//! # What tamper-evident means here, and what it does not
//!
//! Each entry carries the hash of the one before it, so altering or removing
//! any entry breaks every hash after it and [`AuditLog::verify`] reports
//! where. That detects **tampering**, which is the property the plan asks
//! for.
//!
//! It does not *prevent* tampering. Anyone who can write the file can rewrite
//! the whole chain from the altered point and produce a self-consistent one.
//! Preventing that needs the chain head published somewhere the attacker does
//! not control: a signature, an external anchor, an append-only store. This
//! is deliberately the detection half, and calling it more than that would be
//! the exact overclaim this codebase exists to avoid.

use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The hash a chain starts from.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// What happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Who acted. The daemon itself, a client, or a session.
    pub principal: String,
    /// Which session it concerned, if any.
    pub session: Option<String>,
    /// What was attempted.
    pub action: String,
    /// The policy that governed it, so a reader can tell an allowed action
    /// from an unenforced one.
    pub policy: String,
    /// What was decided.
    pub decision: String,
    /// Where it ran.
    pub backend: String,
    /// How it turned out.
    pub result: String,
    /// Seconds since the epoch.
    pub at: u64,
}

impl AuditEvent {
    pub fn now(
        principal: impl Into<String>,
        action: impl Into<String>,
        policy: impl Into<String>,
        decision: impl Into<String>,
    ) -> Self {
        Self {
            principal: principal.into(),
            session: None,
            action: action.into(),
            policy: policy.into(),
            decision: decision.into(),
            backend: "local".to_owned(),
            result: "ok".to_owned(),
            at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }

    pub fn for_session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(session.into());
        self
    }

    pub fn with_result(mut self, result: impl Into<String>) -> Self {
        self.result = result.into();
        self
    }
}

/// One link in the chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Position in the chain, from 1.
    pub sequence: u64,
    /// The hash of the entry before this one.
    pub previous: String,
    /// The hash of this entry, over its sequence, previous hash and event.
    pub hash: String,
    pub event: AuditEvent,
}

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("audit I/O error: {0}")]
    Io(String),
    #[error("audit entry {0} is not valid JSON: {1}")]
    Malformed(u64, String),
}

/// Where a chain stops being trustworthy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integrity {
    /// Every entry hashes to what it claims and links to the one before.
    Intact { entries: u64 },
    /// An entry's own hash does not match its contents: it was edited.
    Altered { sequence: u64 },
    /// An entry does not follow the one before it: something was removed or
    /// reordered.
    BrokenChain { sequence: u64 },
}

impl Integrity {
    pub fn is_intact(&self) -> bool {
        matches!(self, Self::Intact { .. })
    }
}

/// Compute an entry's hash.
///
/// Over the sequence, the previous hash and the event's canonical JSON. The
/// previous hash is what makes it a chain rather than a list of independently
/// forgeable rows.
fn hash_entry(sequence: u64, previous: &str, event: &AuditEvent) -> String {
    let mut hasher = Sha256::new();
    hasher.update(sequence.to_be_bytes());
    hasher.update(previous.as_bytes());
    // serde_json is deterministic for this struct: field order is the
    // declaration order and there are no maps.
    hasher.update(
        serde_json::to_vec(event)
            .expect("an audit event serializes")
            .as_slice(),
    );
    format!("{:x}", hasher.finalize())
}

/// An append-only log on disk, one JSON entry per line.
///
/// A line per entry rather than one JSON document: appending must not require
/// rewriting what is already there, and a truncated write can then damage at
/// most the final entry rather than the whole file.
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append an event, linked to whatever is already there.
    pub fn append(&self, event: AuditEvent) -> Result<AuditEntry, AuditError> {
        let entries = self.read_all()?;
        let (sequence, previous) = match entries.last() {
            Some(last) => (last.sequence + 1, last.hash.clone()),
            None => (1, GENESIS.to_owned()),
        };
        let entry = AuditEntry {
            sequence,
            hash: hash_entry(sequence, &previous, &event),
            previous,
            event,
        };

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AuditError::Io(e.to_string()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| AuditError::Io(e.to_string()))?;
        let mut writer = BufWriter::new(file);
        let line = serde_json::to_string(&entry).map_err(|e| AuditError::Io(e.to_string()))?;
        writeln!(writer, "{line}").map_err(|e| AuditError::Io(e.to_string()))?;
        // Flush before returning: an entry the caller believes is recorded
        // must be on disk, not in a buffer that a crash discards.
        writer.flush().map_err(|e| AuditError::Io(e.to_string()))?;
        Ok(entry)
    }

    pub fn read_all(&self) -> Result<Vec<AuditEntry>, AuditError> {
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(AuditError::Io(error.to_string())),
        };
        let mut entries = Vec::new();
        for (index, line) in std::io::BufReader::new(file).lines().enumerate() {
            let line = line.map_err(|e| AuditError::Io(e.to_string()))?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: AuditEntry = serde_json::from_str(&line)
                .map_err(|e| AuditError::Malformed(index as u64 + 1, e.to_string()))?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Check the chain, reporting the first entry that does not hold.
    pub fn verify(&self) -> Result<Integrity, AuditError> {
        let entries = self.read_all()?;
        let mut previous = GENESIS.to_owned();

        for (index, entry) in entries.iter().enumerate() {
            let expected_sequence = index as u64 + 1;
            if entry.sequence != expected_sequence || entry.previous != previous {
                return Ok(Integrity::BrokenChain {
                    sequence: expected_sequence,
                });
            }
            if hash_entry(entry.sequence, &entry.previous, &entry.event) != entry.hash {
                return Ok(Integrity::Altered {
                    sequence: entry.sequence,
                });
            }
            previous = entry.hash.clone();
        }

        Ok(Integrity::Intact {
            entries: entries.len() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_log(label: &str) -> AuditLog {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "anclave-audit-{label}-{}-{}.jsonl",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        AuditLog::new(path)
    }

    fn event(action: &str) -> AuditEvent {
        AuditEvent::now("daemon", action, "untrusted", "allowed")
    }

    #[test]
    fn an_empty_log_is_intact() {
        let log = temp_log("empty");
        assert_eq!(log.verify().unwrap(), Integrity::Intact { entries: 0 });
    }

    #[test]
    fn entries_are_appended_in_order_and_chained() {
        let log = temp_log("append");
        let first = log.append(event("one")).unwrap();
        let second = log.append(event("two")).unwrap();

        assert_eq!(first.sequence, 1);
        assert_eq!(first.previous, GENESIS);
        assert_eq!(second.sequence, 2);
        assert_eq!(
            second.previous, first.hash,
            "each entry must name the one before it"
        );
        assert!(log.verify().unwrap().is_intact());
        let _ = std::fs::remove_file(log.path());
    }

    /// The property the whole design exists for: editing a recorded event
    /// must be detectable, and the report must say where.
    #[test]
    fn editing_an_entry_is_detected() {
        let log = temp_log("edit");
        log.append(event("granted a credential")).unwrap();
        log.append(event("destroyed a workspace")).unwrap();
        log.append(event("denied a push")).unwrap();
        assert!(log.verify().unwrap().is_intact());

        // Rewrite the middle entry's decision, the way someone covering a
        // trail would.
        let text = std::fs::read_to_string(log.path()).unwrap();
        let tampered = text.replace("destroyed a workspace", "read a file       ");
        assert_ne!(text, tampered, "the test must actually change something");
        std::fs::write(log.path(), tampered).unwrap();

        assert_eq!(log.verify().unwrap(), Integrity::Altered { sequence: 2 });
        let _ = std::fs::remove_file(log.path());
    }

    /// Deleting an inconvenient entry is the other obvious move.
    #[test]
    fn removing_an_entry_is_detected() {
        let log = temp_log("remove");
        for action in ["one", "two", "three"] {
            log.append(event(action)).unwrap();
        }

        let text = std::fs::read_to_string(log.path()).unwrap();
        let kept: Vec<&str> = text
            .lines()
            .enumerate()
            .filter(|(index, _)| *index != 1)
            .map(|(_, line)| line)
            .collect();
        std::fs::write(log.path(), kept.join("\n") + "\n").unwrap();

        // The third entry no longer follows the first.
        assert_eq!(
            log.verify().unwrap(),
            Integrity::BrokenChain { sequence: 2 }
        );
        let _ = std::fs::remove_file(log.path());
    }

    /// Truncating the tail is not detectable by the chain alone, and saying
    /// so is the honest position: every remaining entry still links
    /// correctly. Detecting it needs the head published somewhere the
    /// attacker does not control.
    #[test]
    fn truncating_the_tail_leaves_a_self_consistent_chain() {
        let log = temp_log("truncate");
        for action in ["one", "two", "three"] {
            log.append(event(action)).unwrap();
        }

        let text = std::fs::read_to_string(log.path()).unwrap();
        let kept: Vec<&str> = text.lines().take(2).collect();
        std::fs::write(log.path(), kept.join("\n") + "\n").unwrap();

        assert_eq!(
            log.verify().unwrap(),
            Integrity::Intact { entries: 2 },
            "the chain cannot see its own missing tail: this is a known limit"
        );
        let _ = std::fs::remove_file(log.path());
    }

    /// A crash mid-write damages at most the last line, and the log must say
    /// so rather than silently returning a short history.
    #[test]
    fn a_half_written_entry_is_reported_not_ignored() {
        let log = temp_log("partial");
        log.append(event("one")).unwrap();

        let mut text = std::fs::read_to_string(log.path()).unwrap();
        text.push_str("{\"sequence\":2,\"previous\":\"x\",\"ha");
        std::fs::write(log.path(), text).unwrap();

        assert!(
            matches!(log.verify(), Err(AuditError::Malformed(2, _))),
            "a truncated entry must be reported"
        );
        let _ = std::fs::remove_file(log.path());
    }

    /// The log survives the process that wrote it.
    #[test]
    fn a_log_reopened_continues_its_chain() {
        let log = temp_log("reopen");
        let first = log.append(event("one")).unwrap();

        let reopened = AuditLog::new(log.path());
        let second = reopened.append(event("two")).unwrap();

        assert_eq!(second.previous, first.hash);
        assert!(reopened.verify().unwrap().is_intact());
        let _ = std::fs::remove_file(log.path());
    }

    /// Two identical events must not produce identical entries, or a
    /// duplicate could be swapped for the other without detection.
    #[test]
    fn identical_events_still_hash_differently() {
        let log = temp_log("dupe");
        let first = log.append(event("same")).unwrap();
        let second = log.append(event("same")).unwrap();
        assert_ne!(first.hash, second.hash);
        let _ = std::fs::remove_file(log.path());
    }
}
