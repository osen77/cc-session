use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};

use crate::parser::{ConversationEntry, ConversationSession};

/// Result of a smart merge operation
#[derive(Debug)]
pub struct MergeResult {
    /// The merged conversation entries
    pub merged_entries: Vec<ConversationEntry>,

    /// Statistics about the merge
    pub stats: MergeStats,
}

/// Statistics about a merge operation
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MergeStats {
    /// Number of messages from local
    pub local_messages: usize,

    /// Number of messages from remote
    pub remote_messages: usize,

    /// Number of messages in merged result
    pub merged_messages: usize,

    /// Number of duplicate messages detected
    pub duplicates_removed: usize,

    /// Number of edited messages detected and resolved
    pub edits_resolved: usize,

    /// Number of conversation branches detected
    pub branches_detected: usize,

    /// Number of entries merged by timestamp (non-UUID entries)
    pub timestamp_merged: usize,

    /// Number of unique UUID entries expected from the input union.
    #[serde(default)]
    pub expected_uuid_count: usize,

    /// Number of unique UUID entries emitted by the tree traversal.
    #[serde(default)]
    pub emitted_uuid_count: usize,

    /// Number of roots retained even though their parent UUID was missing.
    #[serde(default)]
    pub orphan_roots_preserved: usize,
}

/// Smart merger for combining conversation sessions
pub struct SmartMerger<'a> {
    local: &'a ConversationSession,
    remote: &'a ConversationSession,
    stats: MergeStats,
}

impl<'a> SmartMerger<'a> {
    /// Creates a new smart merger for the given sessions
    pub fn new(local: &'a ConversationSession, remote: &'a ConversationSession) -> Self {
        SmartMerger {
            local,
            remote,
            stats: MergeStats::default(),
        }
    }

    /// Performs the smart merge and returns the result
    pub fn merge(&mut self) -> Result<MergeResult> {
        // Count initial messages
        self.stats.local_messages = self.local.message_count();
        self.stats.remote_messages = self.remote.message_count();

        // Build UUID maps for both sessions
        let local_map = self.build_uuid_map(&self.local.entries, "local")?;
        let remote_map = self.build_uuid_map(&self.remote.entries, "remote")?;

        // Detect and resolve edits (same UUID, different content)
        let resolved_edits = self.detect_and_resolve_edits(&local_map, &remote_map)?;

        // Separate entries into UUID-tracked and non-UUID entries
        let (local_uuid_entries, local_non_uuid): (Vec<_>, Vec<_>) =
            self.local.entries.iter().partition(|e| e.uuid.is_some());

        let (remote_uuid_entries, remote_non_uuid): (Vec<_>, Vec<_>) =
            self.remote.entries.iter().partition(|e| e.uuid.is_some());

        // Combine all UUID entries from both sides
        let mut all_uuid_entries: Vec<&ConversationEntry> = Vec::new();
        all_uuid_entries.extend(local_uuid_entries);
        all_uuid_entries.extend(remote_uuid_entries);

        // Build one deterministic, non-recursive traversal from all entries.
        let mut merged_entries = self.build_unified_tree(&all_uuid_entries, &resolved_edits)?;

        // Verify UUID conservation before accepting the smart merge.
        let expected_uuids: HashSet<String> =
            local_map.keys().chain(remote_map.keys()).cloned().collect();
        let emitted_uuids: Vec<String> = merged_entries
            .iter()
            .filter_map(|entry| entry.uuid.clone())
            .collect();
        let emitted_set: HashSet<String> = emitted_uuids.iter().cloned().collect();
        self.stats.expected_uuid_count = expected_uuids.len();
        self.stats.emitted_uuid_count = emitted_uuids.len();
        if emitted_uuids.len() != emitted_set.len() || emitted_set != expected_uuids {
            return Err(anyhow!(
                "smart merge UUID integrity check failed: expected {} unique UUIDs, emitted {} entries / {} unique UUIDs",
                expected_uuids.len(),
                emitted_uuids.len(),
                emitted_set.len()
            ));
        }

        // Merge non-UUID entries by timestamp
        let local_vec: Vec<_> = local_non_uuid.into_iter().cloned().collect();
        let remote_vec: Vec<_> = remote_non_uuid.into_iter().cloned().collect();
        let non_uuid_merged = self.merge_by_timestamp(&local_vec, &remote_vec);

        self.stats.timestamp_merged = non_uuid_merged.len();

        // Combine UUID-based and timestamp-based entries, sorted by timestamp
        merged_entries.extend(non_uuid_merged);
        merged_entries.sort_by(|a, b| {
            let a_ts = a.timestamp.as_ref();
            let b_ts = b.timestamp.as_ref();
            a_ts.cmp(&b_ts)
        });

        self.stats.merged_messages = merged_entries.len();

        Ok(MergeResult {
            merged_entries,
            stats: self.stats.clone(),
        })
    }

    /// Builds a UUID map with first-entry canonical semantics. Identical
    /// same-side duplicates are explicitly deduplicated; conflicting duplicates
    /// fail closed before tree construction.
    fn build_uuid_map(
        &mut self,
        entries: &[ConversationEntry],
        side: &str,
    ) -> Result<HashMap<String, ConversationEntry>> {
        let mut map = HashMap::new();
        for entry in entries {
            let Some(uuid) = &entry.uuid else {
                continue;
            };
            if let Some(existing) = map.get(uuid) {
                if serde_json::to_value(existing)? != serde_json::to_value(entry)? {
                    return Err(anyhow!("conflicting duplicate UUID {uuid} on {side} side"));
                }
                self.stats.duplicates_removed += 1;
                continue;
            }
            map.insert(uuid.clone(), entry.clone());
        }
        Ok(map)
    }

    /// Detects edits (same UUID, different content) and resolves them by timestamp
    fn detect_and_resolve_edits(
        &mut self,
        local_map: &HashMap<String, ConversationEntry>,
        remote_map: &HashMap<String, ConversationEntry>,
    ) -> Result<HashMap<String, ConversationEntry>> {
        let mut resolved = HashMap::new();

        // Find all UUIDs that exist in both maps
        let common_uuids: HashSet<_> = local_map
            .keys()
            .filter(|uuid| remote_map.contains_key(*uuid))
            .collect();

        for uuid in common_uuids {
            let local_entry = &local_map[uuid];
            let remote_entry = &remote_map[uuid];

            // Compare content to detect edits
            let local_json = serde_json::to_string(local_entry)?;
            let remote_json = serde_json::to_string(remote_entry)?;

            if local_json != remote_json {
                // Edit detected - resolve by timestamp
                self.stats.edits_resolved += 1;

                let chosen = self.resolve_by_timestamp(local_entry, remote_entry);
                resolved.insert(uuid.clone(), chosen.clone());
            } else {
                // Same content, just add one copy
                resolved.insert(uuid.clone(), local_entry.clone());
            }
        }

        Ok(resolved)
    }

    /// Resolves an edit conflict by choosing the entry with the newer timestamp
    fn resolve_by_timestamp<'b>(
        &self,
        local: &'b ConversationEntry,
        remote: &'b ConversationEntry,
    ) -> &'b ConversationEntry {
        match (&local.timestamp, &remote.timestamp) {
            (Some(local_ts), Some(remote_ts)) => {
                if remote_ts > local_ts {
                    remote
                } else {
                    local
                }
            }
            (Some(_), None) => local,
            (None, Some(_)) => remote,
            (None, None) => local, // Fallback to local if no timestamps
        }
    }

    /// Builds a deterministic depth-first ordering without recursive descent.
    fn build_unified_tree(
        &mut self,
        all_entries: &[&ConversationEntry],
        resolved_edits: &HashMap<String, ConversationEntry>,
    ) -> Result<Vec<ConversationEntry>> {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum VisitState {
            Visiting,
            Visited,
        }

        let mut uuid_to_entry: HashMap<String, ConversationEntry> = HashMap::new();
        let mut ordinals: HashMap<String, usize> = HashMap::new();
        for (ordinal, entry) in all_entries.iter().enumerate() {
            if let Some(uuid) = &entry.uuid {
                ordinals.entry(uuid.clone()).or_insert(ordinal);
                uuid_to_entry
                    .entry(uuid.clone())
                    .or_insert_with(|| resolved_edits.get(uuid).unwrap_or(*entry).clone());
            }
        }

        let mut all_uuids: Vec<String> = uuid_to_entry.keys().cloned().collect();
        all_uuids.sort();
        let mut states: HashMap<String, VisitState> = HashMap::new();
        for start_uuid in &all_uuids {
            if states.get(start_uuid) == Some(&VisitState::Visited) {
                continue;
            }
            let mut path = Vec::new();
            let mut current = start_uuid.clone();
            loop {
                match states.get(&current) {
                    Some(VisitState::Visiting) => {
                        return Err(anyhow!(
                            "circular parentUuid relationship detected at UUID {current}"
                        ));
                    }
                    Some(VisitState::Visited) => break,
                    None => {}
                }
                states.insert(current.clone(), VisitState::Visiting);
                path.push(current.clone());
                let Some(parent) = uuid_to_entry
                    .get(&current)
                    .and_then(|entry| entry.parent_uuid.as_ref())
                    .filter(|parent| uuid_to_entry.contains_key(*parent))
                else {
                    break;
                };
                current = parent.clone();
            }
            for uuid in path {
                states.insert(uuid, VisitState::Visited);
            }
        }

        let sort_uuids = |uuids: &mut Vec<String>| {
            uuids.sort_by(|a, b| {
                let a_entry = &uuid_to_entry[a];
                let b_entry = &uuid_to_entry[b];
                a_entry
                    .timestamp
                    .cmp(&b_entry.timestamp)
                    .then_with(|| ordinals[a].cmp(&ordinals[b]))
                    .then_with(|| a.cmp(b))
            });
        };

        let mut parent_to_children: HashMap<String, Vec<String>> = HashMap::new();
        let mut root_uuids = Vec::new();
        let mut orphan_roots = 0usize;
        for (uuid, entry) in &uuid_to_entry {
            match entry.parent_uuid.as_deref() {
                Some(parent) if uuid_to_entry.contains_key(parent) => {
                    parent_to_children
                        .entry(parent.to_string())
                        .or_default()
                        .push(uuid.clone());
                }
                Some(_) => {
                    orphan_roots += 1;
                    root_uuids.push(uuid.clone());
                }
                None => root_uuids.push(uuid.clone()),
            }
        }
        sort_uuids(&mut root_uuids);
        for children in parent_to_children.values_mut() {
            sort_uuids(children);
            if children.len() > 1 {
                self.stats.branches_detected += 1;
            }
        }
        self.stats.orphan_roots_preserved = orphan_roots;

        let mut ordered = Vec::with_capacity(uuid_to_entry.len());
        let mut stack: Vec<String> = root_uuids.into_iter().rev().collect();
        while let Some(uuid) = stack.pop() {
            ordered.push(uuid_to_entry[&uuid].clone());
            if let Some(children) = parent_to_children.get(&uuid) {
                stack.extend(children.iter().rev().cloned());
            }
        }
        Ok(ordered)
    }

    /// Merges non-UUID entries by timestamp, removing duplicates
    fn merge_by_timestamp(
        &mut self,
        local: &[ConversationEntry],
        remote: &[ConversationEntry],
    ) -> Vec<ConversationEntry> {
        let mut all_entries = local.to_owned();
        all_entries.extend(remote.to_owned());

        // Sort by timestamp
        all_entries.sort_by(|a, b| {
            let a_ts = a.timestamp.as_ref();
            let b_ts = b.timestamp.as_ref();
            a_ts.cmp(&b_ts)
        });

        // Remove duplicates by comparing JSON representation
        let mut seen = HashSet::new();
        let mut unique_entries = Vec::new();

        for entry in all_entries {
            if let Ok(json) = serde_json::to_string(&entry) {
                if seen.insert(json.clone()) {
                    unique_entries.push(entry);
                } else {
                    self.stats.duplicates_removed += 1;
                }
            }
        }

        unique_entries
    }
}

/// Attempts to perform a smart merge on two conversation sessions
///
/// This is the main entry point for the smart merge feature. It will attempt
/// to intelligently combine messages from both conversations, handling:
/// - Non-overlapping messages (simple merge)
/// - Edited messages (resolved by timestamp)
/// - Conversation branches (all branches preserved)
/// - Entries without UUIDs (merged by timestamp)
///
/// # Arguments
///
/// * `local` - The local conversation session
/// * `remote` - The remote conversation session
///
/// # Returns
///
/// Returns `Ok(MergeResult)` if merge succeeds, or an error if the merge
/// cannot be completed (e.g., due to corrupted data or circular references).
pub fn merge_conversations(
    local: &ConversationSession,
    remote: &ConversationSession,
) -> Result<MergeResult> {
    // Validate sessions have same session ID
    if local.session_id != remote.session_id {
        return Err(anyhow!(
            "Cannot merge conversations with different session IDs: {} vs {}",
            local.session_id,
            remote.session_id
        ));
    }

    let mut merger = SmartMerger::new(local, remote);
    merger.merge()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn create_test_entry(
        uuid: &str,
        parent_uuid: Option<&str>,
        timestamp: &str,
    ) -> ConversationEntry {
        ConversationEntry {
            entry_type: "user".to_string(),
            uuid: Some(uuid.to_string()),
            parent_uuid: parent_uuid.map(|s| s.to_string()),
            session_id: Some("test-session".to_string()),
            timestamp: Some(timestamp.to_string()),
            message: Some(json!({"text": format!("Message {}", uuid)})),
            cwd: None,
            version: None,
            git_branch: None,
            custom_title: None,
            extra: serde_json::Value::Null,
        }
    }

    #[test]
    fn test_merge_non_overlapping_messages() {
        // Local has messages 1 -> 2
        let local_entries = vec![
            create_test_entry("1", None, "2025-01-01T00:00:00Z"),
            create_test_entry("2", Some("1"), "2025-01-01T00:01:00Z"),
        ];

        // Remote has the same messages 1 -> 2, plus extensions 3 -> 4
        // This simulates one machine extending the conversation
        let remote_entries = vec![
            create_test_entry("1", None, "2025-01-01T00:00:00Z"),
            create_test_entry("2", Some("1"), "2025-01-01T00:01:00Z"),
            create_test_entry("3", Some("2"), "2025-01-01T00:02:00Z"),
            create_test_entry("4", Some("3"), "2025-01-01T00:03:00Z"),
        ];

        let local = ConversationSession {
            session_id: "test-session".to_string(),
            entries: local_entries,
            file_path: "local.jsonl".to_string(),
        };

        let remote = ConversationSession {
            session_id: "test-session".to_string(),
            entries: remote_entries,
            file_path: "remote.jsonl".to_string(),
        };

        let result = merge_conversations(&local, &remote).unwrap();

        // Should have all 4 messages (local 1,2 are duplicates of remote 1,2)
        assert_eq!(
            result.merged_entries.len(),
            4,
            "Should merge to 4 total messages"
        );
        assert_eq!(result.stats.merged_messages, 4);
        assert_eq!(result.stats.local_messages, 2);
        assert_eq!(result.stats.remote_messages, 4);
    }

    #[test]
    fn test_merge_with_branches() {
        // Local: 1 -> 2 -> 3 (one continuation from message 2)
        let local_entries = vec![
            create_test_entry("1", None, "2025-01-01T00:00:00Z"),
            create_test_entry("2", Some("1"), "2025-01-01T00:01:00Z"),
            create_test_entry("3", Some("2"), "2025-01-01T00:02:00Z"),
        ];

        // Remote: 1 -> 2 -> 4 (different continuation from message 2)
        // Simulates conversation branching - same parent, different children
        let remote_entries = vec![
            create_test_entry("1", None, "2025-01-01T00:00:00Z"),
            create_test_entry("2", Some("1"), "2025-01-01T00:01:00Z"),
            create_test_entry("4", Some("2"), "2025-01-01T00:02:30Z"),
        ];

        let local = ConversationSession {
            session_id: "test-session".to_string(),
            entries: local_entries,
            file_path: "local.jsonl".to_string(),
        };

        let remote = ConversationSession {
            session_id: "test-session".to_string(),
            entries: remote_entries,
            file_path: "remote.jsonl".to_string(),
        };

        let result = merge_conversations(&local, &remote).unwrap();

        // Should detect branch (message 2 has two children: 3 and 4)
        assert!(
            result.stats.branches_detected > 0,
            "Should detect at least one branch"
        );

        // Should have 1, 2, 3, and 4 (all unique messages)
        assert_eq!(
            result.merged_entries.len(),
            4,
            "Should have all 4 unique messages"
        );

        // Verify we have the right messages by UUID
        let uuids: Vec<String> = result
            .merged_entries
            .iter()
            .filter_map(|e| e.uuid.clone())
            .collect();
        assert!(uuids.contains(&"1".to_string()));
        assert!(uuids.contains(&"2".to_string()));
        assert!(uuids.contains(&"3".to_string()));
        assert!(uuids.contains(&"4".to_string()));
    }

    #[test]
    fn test_edit_resolution_by_timestamp() {
        // Same message edited in both places
        let mut local_entry = create_test_entry("1", None, "2025-01-01T00:00:00Z");
        local_entry.message = Some(json!({"text": "Local version"}));

        let mut remote_entry = create_test_entry("1", None, "2025-01-01T00:01:00Z");
        remote_entry.message = Some(json!({"text": "Remote version (newer)"}));

        let local = ConversationSession {
            session_id: "test-session".to_string(),
            entries: vec![local_entry],
            file_path: "local.jsonl".to_string(),
        };

        let remote = ConversationSession {
            session_id: "test-session".to_string(),
            entries: vec![remote_entry],
            file_path: "remote.jsonl".to_string(),
        };

        let result = merge_conversations(&local, &remote).unwrap();

        // Should detect and resolve one edit
        assert_eq!(result.stats.edits_resolved, 1);

        // Should keep only the newer version (remote)
        assert_eq!(result.merged_entries.len(), 1);
        assert_eq!(
            result.merged_entries[0].message,
            Some(json!({"text": "Remote version (newer)"}))
        );
    }
}
