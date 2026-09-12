use claude_code_sync::merge::merge_conversations;
use claude_code_sync::parser::{ConversationEntry, ConversationSession};
use serde_json::json;

/// Helper to create a test entry
fn create_entry(
    uuid: &str,
    parent: Option<&str>,
    timestamp: &str,
    content: &str,
) -> ConversationEntry {
    ConversationEntry {
        entry_type: "user".to_string(),
        uuid: Some(uuid.to_string()),
        parent_uuid: parent.map(|s| s.to_string()),
        session_id: Some("test-session".to_string()),
        timestamp: Some(timestamp.to_string()),
        message: Some(json!({"text": content})),
        cwd: None,
        version: None,
        git_branch: None,
        custom_title: None,
        extra: serde_json::Value::Null,
    }
}

fn write_synthetic_conversation_fixture(path: &std::path::Path) {
    let entries = [
        create_entry("A", None, "2025-01-01T00:00:00Z", "Synthetic message A"),
        create_entry(
            "B",
            Some("A"),
            "2025-01-01T00:01:00Z",
            "Synthetic message B",
        ),
        create_entry(
            "C",
            Some("B"),
            "2025-01-01T00:02:00Z",
            "Synthetic message C",
        ),
        create_entry(
            "D",
            Some("C"),
            "2025-01-01T00:03:00Z",
            "Synthetic message D",
        ),
    ];
    let jsonl = entries
        .iter()
        .map(|entry| serde_json::to_string(entry).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, format!("{jsonl}\n")).unwrap();
}

#[test]
fn test_simple_extension() {
    // Local: A -> B
    let local = ConversationSession {
        session_id: "test".to_string(),
        file_path: "local.jsonl".to_string(),
        entries: vec![
            create_entry("A", None, "2025-01-01T00:00:00Z", "Message A"),
            create_entry("B", Some("A"), "2025-01-01T00:01:00Z", "Message B"),
        ],
    };

    // Remote: A -> B -> C -> D (extended conversation)
    let remote = ConversationSession {
        session_id: "test".to_string(),
        file_path: "remote.jsonl".to_string(),
        entries: vec![
            create_entry("A", None, "2025-01-01T00:00:00Z", "Message A"),
            create_entry("B", Some("A"), "2025-01-01T00:01:00Z", "Message B"),
            create_entry("C", Some("B"), "2025-01-01T00:02:00Z", "Message C"),
            create_entry("D", Some("C"), "2025-01-01T00:03:00Z", "Message D"),
        ],
    };

    let result = merge_conversations(&local, &remote).unwrap();

    println!("Merged {} messages", result.merged_entries.len());
    println!("Stats: {:?}", result.stats);

    for (i, entry) in result.merged_entries.iter().enumerate() {
        println!(
            "Entry {}: UUID={:?}, Parent={:?}",
            i, entry.uuid, entry.parent_uuid
        );
    }

    // Should have A, B, C, D = 4 messages
    assert_eq!(
        result.merged_entries.len(),
        4,
        "Should have 4 messages after merge"
    );
}

#[test]
fn test_conversation_branch() {
    // Local: A -> B -> C
    let local = ConversationSession {
        session_id: "test".to_string(),
        file_path: "local.jsonl".to_string(),
        entries: vec![
            create_entry("A", None, "2025-01-01T00:00:00Z", "Message A"),
            create_entry("B", Some("A"), "2025-01-01T00:01:00Z", "Message B"),
            create_entry(
                "C",
                Some("B"),
                "2025-01-01T00:02:00Z",
                "Message C - local branch",
            ),
        ],
    };

    // Remote: A -> B -> D (different branch from B)
    let remote = ConversationSession {
        session_id: "test".to_string(),
        file_path: "remote.jsonl".to_string(),
        entries: vec![
            create_entry("A", None, "2025-01-01T00:00:00Z", "Message A"),
            create_entry("B", Some("A"), "2025-01-01T00:01:00Z", "Message B"),
            create_entry(
                "D",
                Some("B"),
                "2025-01-01T00:02:30Z",
                "Message D - remote branch",
            ),
        ],
    };

    let result = merge_conversations(&local, &remote).unwrap();

    println!("Merged {} messages", result.merged_entries.len());
    println!("Stats: {:?}", result.stats);
    println!("Branches detected: {}", result.stats.branches_detected);

    // Should have A, B, C, D = 4 messages
    assert_eq!(
        result.merged_entries.len(),
        4,
        "Should have 4 messages with both branches"
    );

    // Should detect that B has two children
    assert!(
        result.stats.branches_detected > 0,
        "Should detect conversation branch"
    );
}

#[test]
fn test_edited_message_resolution() {
    // Local: A with old timestamp
    let local = ConversationSession {
        session_id: "test".to_string(),
        file_path: "local.jsonl".to_string(),
        entries: vec![create_entry(
            "A",
            None,
            "2025-01-01T00:00:00Z",
            "Original message",
        )],
    };

    // Remote: A with newer timestamp (edited)
    let remote = ConversationSession {
        session_id: "test".to_string(),
        file_path: "remote.jsonl".to_string(),
        entries: vec![create_entry(
            "A",
            None,
            "2025-01-01T00:05:00Z",
            "Edited message",
        )],
    };

    let result = merge_conversations(&local, &remote).unwrap();

    // Should have 1 message (the edited one)
    assert_eq!(result.merged_entries.len(), 1);

    // Should have detected and resolved 1 edit
    assert_eq!(result.stats.edits_resolved, 1, "Should detect one edit");

    // Should keep the newer version
    let content = result.merged_entries[0].message.as_ref().unwrap()["text"]
        .as_str()
        .unwrap();
    assert_eq!(content, "Edited message", "Should keep newer version");
}

#[test]
fn test_non_overlapping_additions() {
    // Local adds C, Remote adds D
    // Local: A -> B -> C
    let local = ConversationSession {
        session_id: "test".to_string(),
        file_path: "local.jsonl".to_string(),
        entries: vec![
            create_entry("A", None, "2025-01-01T00:00:00Z", "Message A"),
            create_entry("B", Some("A"), "2025-01-01T00:01:00Z", "Message B"),
            create_entry("C", Some("B"), "2025-01-01T00:02:00Z", "Local addition"),
        ],
    };

    // Remote: A -> B -> D
    let remote = ConversationSession {
        session_id: "test".to_string(),
        file_path: "remote.jsonl".to_string(),
        entries: vec![
            create_entry("A", None, "2025-01-01T00:00:00Z", "Message A"),
            create_entry("B", Some("A"), "2025-01-01T00:01:00Z", "Message B"),
            create_entry("D", Some("B"), "2025-01-01T00:02:30Z", "Remote addition"),
        ],
    };

    let result = merge_conversations(&local, &remote).unwrap();

    // Both additions should be preserved as branches
    assert_eq!(result.merged_entries.len(), 4, "Should have both additions");
    assert!(result.stats.branches_detected > 0, "Should detect branch");
}

#[test]
fn test_synthetic_conversation_file_extension() {
    let temp_dir = tempfile::tempdir().unwrap();
    let test_file = temp_dir.path().join("synthetic-session.jsonl");
    write_synthetic_conversation_fixture(&test_file);

    let session = ConversationSession::from_file(&test_file).unwrap();
    assert_eq!(session.entries.len(), 4);

    // Simulate a scenario where one machine has the first half of messages
    // and another has extended it.
    let midpoint = session.entries.len() / 2;
    let local = ConversationSession {
        session_id: session.session_id.clone(),
        file_path: "local.jsonl".to_string(),
        entries: session.entries[..midpoint].to_vec(),
    };
    let remote = ConversationSession {
        session_id: session.session_id.clone(),
        file_path: "remote.jsonl".to_string(),
        entries: session.entries.clone(),
    };

    let result = merge_conversations(&local, &remote).unwrap();

    assert_eq!(
        result.merged_entries.len(),
        session.entries.len(),
        "Should preserve all messages from extended conversation"
    );
}

#[test]
fn test_complex_branching_scenario() {
    // Create a complex scenario with multiple branches
    //        A
    //       / \
    //      B1  B2
    //     /     \
    //    C1      C2
    //             \
    //              D2

    let local = ConversationSession {
        session_id: "test".to_string(),
        file_path: "local.jsonl".to_string(),
        entries: vec![
            create_entry("A", None, "2025-01-01T00:00:00Z", "Root"),
            create_entry("B1", Some("A"), "2025-01-01T00:01:00Z", "Branch 1 from A"),
            create_entry(
                "C1",
                Some("B1"),
                "2025-01-01T00:02:00Z",
                "Continuation of B1",
            ),
        ],
    };

    let remote = ConversationSession {
        session_id: "test".to_string(),
        file_path: "remote.jsonl".to_string(),
        entries: vec![
            create_entry("A", None, "2025-01-01T00:00:00Z", "Root"),
            create_entry("B2", Some("A"), "2025-01-01T00:01:30Z", "Branch 2 from A"),
            create_entry(
                "C2",
                Some("B2"),
                "2025-01-01T00:02:30Z",
                "Continuation of B2",
            ),
            create_entry(
                "D2",
                Some("C2"),
                "2025-01-01T00:03:00Z",
                "Further continuation",
            ),
        ],
    };

    let result = merge_conversations(&local, &remote).unwrap();

    println!(
        "Complex branching result: {} messages",
        result.merged_entries.len()
    );
    println!("Branches detected: {}", result.stats.branches_detected);

    // Should have all 6 unique messages
    assert_eq!(
        result.merged_entries.len(),
        6,
        "Should have all unique messages"
    );

    // Should detect branch at A (has B1 and B2 as children)
    assert!(
        result.stats.branches_detected > 0,
        "Should detect branching at A"
    );

    // Verify all messages are present
    let uuids: Vec<String> = result
        .merged_entries
        .iter()
        .filter_map(|e| e.uuid.clone())
        .collect();

    for expected_uuid in &["A", "B1", "B2", "C1", "C2", "D2"] {
        assert!(
            uuids.contains(&expected_uuid.to_string()),
            "Should contain message {expected_uuid}"
        );
    }
}

#[test]
fn test_no_conflicts_when_identical() {
    // When both sides have identical conversations, merge should work seamlessly
    let entries = vec![
        create_entry("A", None, "2025-01-01T00:00:00Z", "Message A"),
        create_entry("B", Some("A"), "2025-01-01T00:01:00Z", "Message B"),
        create_entry("C", Some("B"), "2025-01-01T00:02:00Z", "Message C"),
    ];

    let local = ConversationSession {
        session_id: "test".to_string(),
        file_path: "local.jsonl".to_string(),
        entries: entries.clone(),
    };

    let remote = ConversationSession {
        session_id: "test".to_string(),
        file_path: "remote.jsonl".to_string(),
        entries: entries.clone(),
    };

    let result = merge_conversations(&local, &remote).unwrap();

    // Should have exact same messages (no duplicates)
    assert_eq!(
        result.merged_entries.len(),
        3,
        "Should deduplicate identical messages"
    );
    assert_eq!(
        result.stats.duplicates_removed, 0,
        "No duplicates to remove (deduplicated during merge)"
    );
    assert_eq!(
        result.stats.branches_detected, 0,
        "No branches in linear conversation"
    );
}

#[test]
fn test_mixed_uuid_and_non_uuid_entries() {
    // Test merging with both UUID-tracked messages and non-UUID system events
    let local = ConversationSession {
        session_id: "test".to_string(),
        file_path: "local.jsonl".to_string(),
        entries: vec![
            create_entry("A", None, "2025-01-01T00:00:00Z", "User message"),
            ConversationEntry {
                entry_type: "file-history-snapshot".to_string(),
                uuid: None, // System events may not have UUIDs
                parent_uuid: None,
                session_id: Some("test".to_string()),
                timestamp: Some("2025-01-01T00:00:30Z".to_string()),
                message: Some(json!({"snapshot": "data"})),
                cwd: None,
                version: None,
                git_branch: None,
                custom_title: None,
                extra: serde_json::Value::Null,
            },
            create_entry("B", Some("A"), "2025-01-01T00:01:00Z", "Assistant response"),
        ],
    };

    let remote = ConversationSession {
        session_id: "test".to_string(),
        file_path: "remote.jsonl".to_string(),
        entries: vec![
            create_entry("A", None, "2025-01-01T00:00:00Z", "User message"),
            create_entry("B", Some("A"), "2025-01-01T00:01:00Z", "Assistant response"),
            create_entry("C", Some("B"), "2025-01-01T00:02:00Z", "Continuation"),
        ],
    };

    let result = merge_conversations(&local, &remote).unwrap();

    println!(
        "Mixed entries result: {} messages",
        result.merged_entries.len()
    );
    println!("Timestamp-merged: {}", result.stats.timestamp_merged);

    // Should have A, snapshot, B, C = 4 entries
    assert_eq!(
        result.merged_entries.len(),
        4,
        "Should merge UUID and non-UUID entries"
    );
    assert!(
        result.stats.timestamp_merged > 0,
        "Should use timestamp merging for non-UUID entries"
    );
}

#[test]
fn preserves_missing_parent_orphan_subtree_without_reparenting() {
    let local = ConversationSession {
        session_id: "test".to_string(),
        file_path: "local.jsonl".to_string(),
        entries: vec![
            create_entry("root", None, "2025-01-01T00:00:00Z", "root"),
            create_entry(
                "orphan",
                Some("missing-parent"),
                "2025-01-01T00:01:00Z",
                "orphan",
            ),
            create_entry(
                "orphan-child",
                Some("orphan"),
                "2025-01-01T00:02:00Z",
                "orphan child",
            ),
        ],
    };
    let remote = ConversationSession {
        session_id: "test".to_string(),
        file_path: "remote.jsonl".to_string(),
        entries: vec![],
    };

    let result = merge_conversations(&local, &remote).unwrap();
    let uuids = result
        .merged_entries
        .iter()
        .filter_map(|entry| entry.uuid.as_deref())
        .collect::<Vec<_>>();

    assert_eq!(uuids, vec!["root", "orphan", "orphan-child"]);
    assert_eq!(
        result.merged_entries[1].parent_uuid.as_deref(),
        Some("missing-parent")
    );
    assert_eq!(result.stats.expected_uuid_count, 3);
    assert_eq!(result.stats.emitted_uuid_count, 3);
    assert_eq!(result.stats.orphan_roots_preserved, 1);
}

#[test]
fn preserves_multiple_orphan_branches_and_cross_side_parent_links() {
    let local = ConversationSession {
        session_id: "test".to_string(),
        file_path: "local.jsonl".to_string(),
        entries: vec![
            create_entry(
                "child",
                Some("remote-parent"),
                "2025-01-01T00:02:00Z",
                "child",
            ),
            create_entry("orphan-a", Some("missing-a"), "2025-01-01T00:03:00Z", "a"),
            create_entry("orphan-b", Some("missing-b"), "2025-01-01T00:04:00Z", "b"),
        ],
    };
    let remote = ConversationSession {
        session_id: "test".to_string(),
        file_path: "remote.jsonl".to_string(),
        entries: vec![create_entry(
            "remote-parent",
            None,
            "2025-01-01T00:01:00Z",
            "parent",
        )],
    };

    let result = merge_conversations(&local, &remote).unwrap();
    let uuids = result
        .merged_entries
        .iter()
        .filter_map(|entry| entry.uuid.as_deref())
        .collect::<std::collections::HashSet<_>>();

    assert_eq!(
        uuids,
        std::collections::HashSet::from(["remote-parent", "child", "orphan-a", "orphan-b"])
    );
    assert_eq!(result.stats.expected_uuid_count, 4);
    assert_eq!(result.stats.emitted_uuid_count, 4);
    assert_eq!(result.stats.orphan_roots_preserved, 2);
}

#[test]
fn rejects_self_and_multi_node_parent_cycles() {
    for entries in [
        vec![create_entry(
            "self",
            Some("self"),
            "2025-01-01T00:00:00Z",
            "self",
        )],
        vec![
            create_entry("a", Some("b"), "2025-01-01T00:00:00Z", "a"),
            create_entry("b", Some("c"), "2025-01-01T00:01:00Z", "b"),
            create_entry("c", Some("a"), "2025-01-01T00:02:00Z", "c"),
        ],
    ] {
        let local = ConversationSession {
            session_id: "test".to_string(),
            file_path: "local.jsonl".to_string(),
            entries,
        };
        let remote = ConversationSession {
            session_id: "test".to_string(),
            file_path: "remote.jsonl".to_string(),
            entries: vec![],
        };

        let error = merge_conversations(&local, &remote).unwrap_err();
        assert!(error.to_string().contains("circular parentUuid"));
    }
}

#[test]
fn equal_and_missing_timestamps_have_deterministic_uuid_order() {
    let mut no_timestamp_b = create_entry("b", None, "unused", "b");
    no_timestamp_b.timestamp = None;
    let mut no_timestamp_a = create_entry("a", None, "unused", "a");
    no_timestamp_a.timestamp = None;
    let local = ConversationSession {
        session_id: "test".to_string(),
        file_path: "local.jsonl".to_string(),
        entries: vec![
            no_timestamp_b,
            no_timestamp_a,
            create_entry("same-b", None, "2025-01-01T00:00:00Z", "same b"),
            create_entry("same-a", None, "2025-01-01T00:00:00Z", "same a"),
        ],
    };
    let remote = ConversationSession {
        session_id: "test".to_string(),
        file_path: "remote.jsonl".to_string(),
        entries: vec![],
    };

    let expected = merge_conversations(&local, &remote)
        .unwrap()
        .merged_entries
        .iter()
        .map(|entry| entry.uuid.clone().unwrap())
        .collect::<Vec<_>>();
    for _ in 0..20 {
        let actual = merge_conversations(&local, &remote)
            .unwrap()
            .merged_entries
            .iter()
            .map(|entry| entry.uuid.clone().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }
    assert_eq!(expected, vec!["b", "a", "same-b", "same-a"]);
}

#[test]
fn rejects_conflicting_duplicate_uuid_on_the_same_side() {
    let local = ConversationSession {
        session_id: "test".to_string(),
        file_path: "local.jsonl".to_string(),
        entries: vec![
            create_entry("dup", None, "2025-01-01T00:00:00Z", "first"),
            create_entry(
                "dup",
                Some("different-parent"),
                "2025-01-01T00:01:00Z",
                "second",
            ),
        ],
    };
    let remote = ConversationSession {
        session_id: "test".to_string(),
        file_path: "remote.jsonl".to_string(),
        entries: vec![],
    };

    let error = merge_conversations(&local, &remote).unwrap_err();
    assert!(error.to_string().contains("duplicate UUID dup"));
}

#[test]
fn explicitly_deduplicates_identical_same_side_uuid_entries() {
    let duplicate = create_entry("dup", None, "2025-01-01T00:00:00Z", "same");
    let local = ConversationSession {
        session_id: "test".to_string(),
        file_path: "local.jsonl".to_string(),
        entries: vec![duplicate.clone(), duplicate],
    };
    let remote = ConversationSession {
        session_id: "test".to_string(),
        file_path: "remote.jsonl".to_string(),
        entries: vec![],
    };

    let result = merge_conversations(&local, &remote).unwrap();
    assert_eq!(result.merged_entries.len(), 1);
    assert_eq!(result.stats.duplicates_removed, 1);
}

#[test]
fn merges_twenty_thousand_deep_parent_chain_without_recursion() {
    let entries = (0..20_000)
        .map(|index| {
            create_entry(
                &format!("node-{index:05}"),
                (index > 0)
                    .then(|| format!("node-{:05}", index - 1))
                    .as_deref(),
                &format!("2025-01-01T00:{:05}:00Z", index),
                "deep",
            )
        })
        .collect();
    let local = ConversationSession {
        session_id: "test".to_string(),
        file_path: "local.jsonl".to_string(),
        entries,
    };
    let remote = ConversationSession {
        session_id: "test".to_string(),
        file_path: "remote.jsonl".to_string(),
        entries: vec![],
    };

    let result = merge_conversations(&local, &remote).unwrap();
    assert_eq!(result.stats.expected_uuid_count, 20_000);
    assert_eq!(result.stats.emitted_uuid_count, 20_000);
    assert_eq!(result.merged_entries.len(), 20_000);
}

#[test]
fn merge_stats_deserializes_legacy_payload_with_new_fields_defaulted() {
    let stats: claude_code_sync::merge::MergeStats = serde_json::from_value(json!({
        "local_messages": 1,
        "remote_messages": 2,
        "merged_messages": 3,
        "duplicates_removed": 0,
        "edits_resolved": 0,
        "branches_detected": 0,
        "timestamp_merged": 0
    }))
    .unwrap();
    assert_eq!(stats.expected_uuid_count, 0);
    assert_eq!(stats.emitted_uuid_count, 0);
    assert_eq!(stats.orphan_roots_preserved, 0);
}
