use crate::session_cache::FileFingerprint;
use crate::session_model::SessionIdentity;
use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// Consumed by the maintenance state layer when classifier results are persisted.
#[allow(dead_code)]
pub(crate) const CLASSIFIER_VERSION: u32 = 1;
pub(crate) const DEFAULT_THRESHOLD: u16 = 70;
/// Weight of a session that ran inside a temporary root. Set so that a temporary
/// cwd plus one other weak signal still stays below the threshold, while the
/// usual throwaway shape (few messages, short duration) crosses it.
const TEMPORARY_CWD_SCORE: u16 = 40;
/// Weight of an opening message that repeats verbatim in a tight burst.
const REPEATED_TITLE_SCORE: u16 = 30;
/// How many verbatim repeats inside the window make a burst.
const REPEATED_TITLE_BURST_MIN: usize = 3;
/// Window that separates a person re-running the same probe from a schedule.
///
/// Measured against real data: manual re-runs land three copies within minutes,
/// while the tightest realistic schedule (hourly) needs two hours for three runs.
/// Only a sub-half-hourly job with a completely fixed prompt would be caught, and
/// such a job produces enough sessions to be noticed on its own.
const REPEATED_TITLE_WINDOW_MINUTES: i64 = 60;
/// Placeholder used when a session has no extractable opening message. It is not a
/// real title, so it must never group sessions together.
const MISSING_TITLE_PLACEHOLDER: &str = "(No title)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifierPolicy {
    pub threshold: u16,
    pub hide_after_hours: u64,
    pub(crate) temporary_roots: Vec<PathBuf>,
}

impl ClassifierPolicy {
    pub(crate) fn conservative(hide_after_hours: u64) -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            hide_after_hours,
            temporary_roots: default_temporary_roots(),
        }
    }

    pub(crate) fn with_temporary_roots(
        hide_after_hours: u64,
        temporary_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            hide_after_hours,
            temporary_roots,
        }
    }
}

fn default_temporary_roots() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/tmp"),
        PathBuf::from("/private/tmp"),
        PathBuf::from("/var/tmp"),
        PathBuf::from(r"C:\Temp"),
        PathBuf::from(r"C:\Windows\Temp"),
    ]
}

#[allow(dead_code)]
fn _classifier_api_anchor() {
    let _ = ClassifierPolicy::conservative;
    let _ = ClassifierPolicy::with_temporary_roots;
    let _ = classify;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    Keep,
    TestCandidate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    ExplicitTestMarker,
    FixtureSessionId,
    FixtureTemporaryCwd,
    ExactTestTitle,
    AutomatedValidationTitle,
    FewUserMessages,
    FewTotalMessages,
    ShortDuration,
    TemporaryCwd,
    RepeatedTitleBurst,
    RecentActivityProtection,
    CustomTitleProtection,
    LongConversationProtection,
    KeepProtection,
}

#[derive(Debug, Clone)]
pub struct MaintenanceCandidate {
    pub identity: SessionIdentity,
    #[allow(dead_code)]
    pub original_relative_path: PathBuf,
    #[allow(dead_code)]
    pub project_name: String,
    /// Encoded storage directory of the session. Never use this for temporary-root
    /// checks: for Claude it is `~/.claude/projects/<encoded>`, not the real cwd.
    #[allow(dead_code)]
    pub project_dir: PathBuf,
    /// Real working directory the session ran in, when the source records one.
    pub cwd: Option<PathBuf>,
    /// Whether this session's opening message repeats verbatim in a tight burst.
    /// Computed across the whole candidate set by [`repeated_title_bursts`].
    pub repeated_title_burst: bool,
    pub title: String,
    pub has_custom_title: bool,
    pub user_message_count: usize,
    pub message_count: usize,
    pub first_activity: Option<DateTime<Utc>>,
    pub last_activity: Option<DateTime<Utc>>,
    #[allow(dead_code)]
    pub size: u64,
    #[allow(dead_code)]
    pub fingerprint: FileFingerprint,
    pub explicit_test: bool,
    pub keep: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationDecision {
    pub classification: Classification,
    pub score: u16,
    pub reasons: Vec<ReasonCode>,
}

/// Classify one session using the conservative, protection-first policy.
pub(crate) fn classify(
    candidate: &MaintenanceCandidate,
    policy: &ClassifierPolicy,
    now: DateTime<Utc>,
) -> ClassificationDecision {
    if candidate.keep {
        return keep_decision(ReasonCode::KeepProtection);
    }
    if candidate.has_custom_title {
        return keep_decision(ReasonCode::CustomTitleProtection);
    }
    if is_recent(candidate.last_activity, now, policy.hide_after_hours) {
        return keep_decision(ReasonCode::RecentActivityProtection);
    }
    if candidate.message_count > 20 {
        return keep_decision(ReasonCode::LongConversationProtection);
    }
    if conversation_duration(candidate).is_some_and(|duration| duration > Duration::hours(2)) {
        return keep_decision(ReasonCode::LongConversationProtection);
    }

    if candidate.explicit_test {
        return ClassificationDecision {
            classification: Classification::TestCandidate,
            score: 100,
            reasons: vec![ReasonCode::ExplicitTestMarker],
        };
    }

    let mut score = 0_u16;
    let mut reasons = Vec::new();

    if is_fixture_session_id(&candidate.identity.session_id) {
        score = score.saturating_add(60);
        reasons.push(ReasonCode::FixtureSessionId);
    }

    let normalized_title = candidate.title.trim().to_lowercase();
    if is_trivial_probe_title(&normalized_title) {
        score = score.saturating_add(35);
        reasons.push(ReasonCode::ExactTestTitle);
    }

    if ["fixture", "smoke test", "test brief"]
        .iter()
        .any(|keyword| normalized_title.contains(keyword))
    {
        score = score.saturating_add(25);
        reasons.push(ReasonCode::AutomatedValidationTitle);
    }

    if candidate.user_message_count <= 2 {
        score = score.saturating_add(20);
        reasons.push(ReasonCode::FewUserMessages);
    }
    if candidate.message_count <= 6 {
        score = score.saturating_add(10);
        reasons.push(ReasonCode::FewTotalMessages);
    }
    if conversation_duration(candidate)
        .is_some_and(|duration| (Duration::zero()..=Duration::minutes(15)).contains(&duration))
    {
        score = score.saturating_add(15);
        reasons.push(ReasonCode::ShortDuration);
    }

    if candidate.repeated_title_burst {
        score = score.saturating_add(REPEATED_TITLE_SCORE);
        reasons.push(ReasonCode::RepeatedTitleBurst);
    }

    if is_temporary_cwd(candidate.cwd.as_deref(), &policy.temporary_roots) {
        score = score.saturating_add(TEMPORARY_CWD_SCORE);
        if is_fixture_session_id(&candidate.identity.session_id)
            && candidate.cwd.as_deref().is_some_and(is_fixture_cwd)
        {
            reasons.push(ReasonCode::FixtureTemporaryCwd);
        } else {
            reasons.push(ReasonCode::TemporaryCwd);
        }
    }

    let classification = if score >= policy.threshold {
        Classification::TestCandidate
    } else {
        Classification::Keep
    };
    ClassificationDecision {
        classification,
        score,
        reasons,
    }
}

fn keep_decision(reason: ReasonCode) -> ClassificationDecision {
    ClassificationDecision {
        classification: Classification::Keep,
        score: 0,
        reasons: vec![reason],
    }
}

fn is_recent(
    last_activity: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    hide_after_hours: u64,
) -> bool {
    let Some(last_activity) = last_activity else {
        return false;
    };
    let max_hours = (i64::MAX as u64) / 3_600;
    let hide_after = Duration::hours(hide_after_hours.min(max_hours) as i64);
    now.signed_duration_since(last_activity) < hide_after
}

fn conversation_duration(candidate: &MaintenanceCandidate) -> Option<Duration> {
    Some(
        candidate
            .last_activity?
            .signed_duration_since(candidate.first_activity?),
    )
}

fn is_fixture_session_id(session_id: &str) -> bool {
    static FIXTURE_ID: OnceLock<Regex> = OnceLock::new();
    FIXTURE_ID
        .get_or_init(|| {
            Regex::new(r"^(cc|cx|om)(-cache)?-task[0-9]+$").expect("valid fixture ID regex")
        })
        .is_match(session_id)
}

/// Flag each session whose opening message repeats verbatim at least
/// [`REPEATED_TITLE_BURST_MIN`] times inside [`REPEATED_TITLE_WINDOW_MINUTES`].
///
/// A person debugging re-runs the same prompt back to back; a scheduled job repeats
/// on a cadence measured in hours or days. The window is what separates them, so
/// grouping is exact and a job whose prompt embeds per-run content never groups at all.
///
/// Input is `(title, first_activity)` per session, in any order. Output is a
/// same-length, same-order flag vector. Sessions without a timestamp or with the
/// missing-title placeholder are never flagged.
pub(crate) fn repeated_title_bursts(sessions: &[(String, Option<DateTime<Utc>>)]) -> Vec<bool> {
    let mut by_title: HashMap<&str, Vec<(DateTime<Utc>, usize)>> = HashMap::new();
    for (index, (title, first_activity)) in sessions.iter().enumerate() {
        let trimmed = title.trim();
        if trimmed.is_empty() || trimmed == MISSING_TITLE_PLACEHOLDER {
            continue;
        }
        if let Some(started_at) = first_activity {
            by_title
                .entry(trimmed)
                .or_default()
                .push((*started_at, index));
        }
    }

    let window = Duration::minutes(REPEATED_TITLE_WINDOW_MINUTES);
    let mut flags = vec![false; sessions.len()];
    for occurrences in by_title.values_mut() {
        if occurrences.len() < REPEATED_TITLE_BURST_MIN {
            continue;
        }
        occurrences.sort_by_key(|(started_at, _)| *started_at);
        let mut start = 0usize;
        for end in 0..occurrences.len() {
            while occurrences[end]
                .0
                .signed_duration_since(occurrences[start].0)
                > window
            {
                start += 1;
            }
            if end - start + 1 >= REPEATED_TITLE_BURST_MIN {
                for (_, index) in &occurrences[start..=end] {
                    flags[*index] = true;
                }
            }
        }
    }
    flags
}

/// Opening messages that carry no task on their own, so the whole session is a probe.
///
/// Matching is exact after trimming trailing punctuation. Substring matching is
/// deliberately avoided: short genuine questions ("茅台现在多少钱？") share the same
/// message-count and duration shape as a probe, so the title is the only signal
/// separating them and it has to be precise.
const TRIVIAL_PROBE_TITLES: [&str; 11] = [
    "测试",
    "test",
    "hello",
    "hi",
    "试一下",
    "ok",
    "好",
    "你好",
    "在吗",
    "说一句话",
    "随便说点什么",
];

fn is_trivial_probe_title(normalized_title: &str) -> bool {
    let trimmed = normalized_title.trim_end_matches(|character: char| {
        character.is_whitespace()
            || matches!(
                character,
                '。' | '．' | '.' | '！' | '!' | '？' | '?' | '~' | '～' | '、' | '，' | ','
            )
    });
    TRIVIAL_PROBE_TITLES.contains(&trimmed)
}

fn is_temporary_cwd(cwd: Option<&Path>, temporary_roots: &[PathBuf]) -> bool {
    let Some(cwd) = cwd else {
        return false;
    };
    temporary_roots
        .iter()
        .any(|root| !root.as_os_str().is_empty() && cwd.starts_with(root))
}

fn is_fixture_cwd(cwd: &Path) -> bool {
    let Some(name) = cwd.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(task_prefix) = name.strip_prefix("task") else {
        return false;
    };
    let Some(task_number) = task_prefix.strip_suffix("-project") else {
        return false;
    };
    !task_number.is_empty()
        && task_number
            .chars()
            .all(|character| character.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_cache::FileFingerprint;
    use crate::session_model::{SessionIdentity, SessionSource};
    use chrono::{DateTime, Duration, Utc};
    use std::path::PathBuf;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-08T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn policy() -> ClassifierPolicy {
        ClassifierPolicy::with_temporary_roots(24, vec![PathBuf::from("/tmp")])
    }

    fn candidate(
        title: &str,
        user_message_count: usize,
        message_count: usize,
        duration_minutes: i64,
        session_id: &str,
    ) -> MaintenanceCandidate {
        let last_activity = now() - Duration::hours(48);
        let first_activity = last_activity - Duration::minutes(duration_minutes);
        MaintenanceCandidate {
            identity: SessionIdentity {
                source: SessionSource::Claude,
                session_id: session_id.to_string(),
            },
            original_relative_path: PathBuf::from("project/session.jsonl"),
            project_name: "project".to_string(),
            project_dir: PathBuf::from("/Users/example/project"),
            cwd: Some(PathBuf::from("/Users/example/project")),
            repeated_title_burst: false,
            title: title.to_string(),
            has_custom_title: false,
            user_message_count,
            message_count,
            first_activity: Some(first_activity),
            last_activity: Some(last_activity),
            size: 128,
            fingerprint: FileFingerprint {
                digest: "fingerprint".to_string(),
                bytes: 128,
            },
            explicit_test: false,
            keep: false,
        }
    }

    #[test]
    fn exact_test_title_alone_does_not_cross_threshold() {
        let candidate = candidate("test", 5, 3, 60, "550e8400-e29b-41d4-a716-446655440000");
        let decision = classify(&candidate, &policy(), now());
        assert_eq!(decision.classification, Classification::Keep);
        assert!(decision.reasons.contains(&ReasonCode::ExactTestTitle));
    }

    #[test]
    fn multiple_low_value_signals_cross_threshold() {
        let candidate = candidate("test", 2, 1, 5, "550e8400-e29b-41d4-a716-446655440000");
        let decision = classify(&candidate, &policy(), now());
        assert_eq!(decision.classification, Classification::TestCandidate);
        assert_eq!(decision.score, 80);
    }

    #[test]
    fn custom_title_and_recent_activity_are_hard_protections() {
        let mut custom = candidate("test", 2, 1, 5, "cc-task4");
        custom.has_custom_title = true;
        assert_eq!(
            classify(&custom, &policy(), now()).classification,
            Classification::Keep
        );

        let mut recent = candidate("test", 2, 1, 5, "cc-task4");
        recent.last_activity = Some(now() - chrono::Duration::hours(2));
        assert_eq!(
            classify(&recent, &policy(), now()).classification,
            Classification::Keep
        );
    }

    #[test]
    fn explicit_keep_overrides_explicit_test_marker() {
        let mut candidate = candidate("test", 2, 1, 5, "cc-task4");
        candidate.explicit_test = true;
        candidate.keep = true;
        assert_eq!(
            classify(&candidate, &policy(), now()).classification,
            Classification::Keep
        );
    }

    #[test]
    fn score_at_threshold_is_a_test_candidate() {
        let candidate = candidate("test", 2, 7, 5, "ordinary-session");
        let decision = classify(&candidate, &policy(), now());
        assert_eq!(decision.classification, Classification::TestCandidate);
        assert_eq!(decision.score, 70);
    }

    #[test]
    fn long_conversation_protection_covers_message_count_over_twenty() {
        let candidate = candidate("test", 2, 21, 5, "ordinary-session");
        let decision = classify(&candidate, &policy(), now());
        assert_eq!(decision.classification, Classification::Keep);
        assert!(decision
            .reasons
            .contains(&ReasonCode::LongConversationProtection));
    }

    #[test]
    fn long_conversation_protection_covers_duration_over_two_hours() {
        let candidate = candidate("test", 2, 1, 121, "ordinary-session");
        let decision = classify(&candidate, &policy(), now());
        assert_eq!(decision.classification, Classification::Keep);
        assert!(decision
            .reasons
            .contains(&ReasonCode::LongConversationProtection));
    }

    #[test]
    fn fixture_id_regex_accepts_only_supported_task_shapes() {
        for session_id in ["cc-task4", "cx-cache-task9", "om-task1"] {
            let candidate = candidate("ordinary", 5, 3, 60, session_id);
            let decision = classify(&candidate, &policy(), now());
            assert!(decision.reasons.contains(&ReasonCode::FixtureSessionId));
        }
        for session_id in ["cc-task", "cc-task4-extra", "claude-task4", "cc-taskx"] {
            let candidate = candidate("ordinary", 5, 3, 60, session_id);
            let decision = classify(&candidate, &policy(), now());
            assert!(!decision.reasons.contains(&ReasonCode::FixtureSessionId));
        }
    }

    #[test]
    fn fixture_temporary_cwd_has_dedicated_reason() {
        let mut candidate = candidate("ordinary", 5, 3, 60, "cc-task4");
        candidate.cwd = Some(PathBuf::from("/tmp/task3-project"));
        let decision = classify(&candidate, &policy(), now());
        assert!(decision.reasons.contains(&ReasonCode::FixtureTemporaryCwd));
    }

    #[test]
    fn automated_validation_title_alone_does_not_cross_threshold() {
        let candidate = candidate("smoke test", 5, 3, 60, "ordinary-session");
        let decision = classify(&candidate, &policy(), now());
        assert_eq!(decision.classification, Classification::Keep);
        assert!(decision.score < DEFAULT_THRESHOLD);
        assert!(decision
            .reasons
            .contains(&ReasonCode::AutomatedValidationTitle));
    }

    #[test]
    fn explicit_test_marker_scores_one_hundred() {
        let mut candidate = candidate("ordinary", 5, 3, 60, "ordinary-session");
        candidate.explicit_test = true;
        let decision = classify(&candidate, &policy(), now());
        assert_eq!(decision.classification, Classification::TestCandidate);
        assert_eq!(decision.score, 100);
        assert_eq!(decision.reasons, vec![ReasonCode::ExplicitTestMarker]);
    }

    fn burst_input(entries: &[(&str, i64)]) -> Vec<(String, Option<DateTime<Utc>>)> {
        entries
            .iter()
            .map(|(title, offset_minutes)| {
                (
                    (*title).to_string(),
                    Some(now() + Duration::minutes(*offset_minutes)),
                )
            })
            .collect()
    }

    #[test]
    fn three_identical_titles_within_the_window_are_a_burst() {
        let sessions = burst_input(&[("列出工具名", 0), ("列出工具名", 2), ("列出工具名", 33)]);
        assert_eq!(repeated_title_bursts(&sessions), vec![true, true, true]);
    }

    #[test]
    fn daily_scheduled_repeats_are_not_a_burst() {
        // A job firing once a day repeats verbatim but never lands three runs
        // inside the window, so it must stay untouched.
        let day = 24 * 60;
        let sessions = burst_input(&[("每日巡检", 0), ("每日巡检", day), ("每日巡检", 2 * day)]);
        assert_eq!(repeated_title_bursts(&sessions), vec![false, false, false]);
    }

    #[test]
    fn hourly_scheduled_repeats_are_not_a_burst() {
        let sessions = burst_input(&[("每小时巡检", 0), ("每小时巡检", 60), ("每小时巡检", 120)]);
        assert_eq!(repeated_title_bursts(&sessions), vec![false, false, false]);
    }

    #[test]
    fn only_the_clustered_members_of_a_group_are_flagged() {
        let sessions = burst_input(&[
            ("同一提示词", 0),
            ("同一提示词", 1),
            ("同一提示词", 2),
            ("同一提示词", 10_000),
        ]);
        assert_eq!(
            repeated_title_bursts(&sessions),
            vec![true, true, true, false]
        );
    }

    #[test]
    fn two_repeats_are_never_a_burst() {
        let sessions = burst_input(&[("茅台现在多少钱？", 0), ("茅台现在多少钱？", 9)]);
        assert_eq!(repeated_title_bursts(&sessions), vec![false, false]);
    }

    #[test]
    fn placeholder_and_untimed_sessions_are_never_a_burst() {
        let sessions = vec![
            ("(No title)".to_string(), Some(now())),
            ("(No title)".to_string(), Some(now() + Duration::minutes(1))),
            ("(No title)".to_string(), Some(now() + Duration::minutes(2))),
            ("未知时间".to_string(), None),
            ("未知时间".to_string(), None),
            ("未知时间".to_string(), None),
        ];
        assert_eq!(
            repeated_title_bursts(&sessions),
            vec![false, false, false, false, false, false]
        );
    }

    #[test]
    fn repeated_title_burst_crosses_threshold_only_with_the_throwaway_shape() {
        let mut probe = candidate(
            "列出工具名",
            1,
            2,
            1,
            "550e8400-e29b-41d4-a716-446655440000",
        );
        probe.repeated_title_burst = true;
        let decision = classify(&probe, &policy(), now());
        assert!(decision.reasons.contains(&ReasonCode::RepeatedTitleBurst));
        assert_eq!(decision.classification, Classification::TestCandidate);

        // A substantial conversation that happens to repeat stays below the line.
        let mut substantial = candidate(
            "列出工具名",
            5,
            12,
            90,
            "550e8400-e29b-41d4-a716-446655440000",
        );
        substantial.repeated_title_burst = true;
        let decision = classify(&substantial, &policy(), now());
        assert_eq!(decision.classification, Classification::Keep);
    }

    #[test]
    fn trivial_probe_titles_ignore_trailing_punctuation() {
        for title in ["ok", "OK", "说一句话。", "你好！", "在吗?", "好"] {
            let candidate = candidate(title, 1, 2, 1, "550e8400-e29b-41d4-a716-446655440000");
            let decision = classify(&candidate, &policy(), now());
            assert!(
                decision.reasons.contains(&ReasonCode::ExactTestTitle),
                "expected {title:?} to be a trivial probe"
            );
            assert_eq!(decision.classification, Classification::TestCandidate);
        }
    }

    #[test]
    fn real_questions_that_merely_start_with_a_probe_word_are_kept() {
        // Short genuine questions share the throwaway shape, so only an exact
        // match may contribute the title score.
        for title in [
            "茅台现在多少钱？",
            "ok 了吗，部署完成没有",
            "你好，帮我看下这个报错",
        ] {
            let candidate = candidate(title, 1, 2, 1, "550e8400-e29b-41d4-a716-446655440000");
            let decision = classify(&candidate, &policy(), now());
            assert!(
                !decision.reasons.contains(&ReasonCode::ExactTestTitle),
                "expected {title:?} to keep its title score at zero"
            );
            assert_eq!(decision.classification, Classification::Keep);
        }
    }

    #[test]
    fn temporary_detection_uses_real_cwd_not_encoded_project_dir() {
        // Production shape: a Claude session started in /tmp is stored under an
        // encoded projects directory that never starts with a temporary root.
        let mut candidate = candidate(
            "只回答一行",
            1,
            2,
            1,
            "550e8400-e29b-41d4-a716-446655440000",
        );
        candidate.project_dir = PathBuf::from("/Users/example/.claude/projects/-tmp");
        candidate.cwd = Some(PathBuf::from("/tmp"));

        let decision = classify(&candidate, &policy(), now());

        assert!(decision.reasons.contains(&ReasonCode::TemporaryCwd));
        assert_eq!(decision.classification, Classification::TestCandidate);
    }

    #[test]
    fn missing_cwd_never_counts_as_temporary() {
        let mut candidate = candidate("ordinary", 1, 2, 1, "550e8400-e29b-41d4-a716-446655440000");
        candidate.project_dir = PathBuf::from("/tmp/project");
        candidate.cwd = None;

        let decision = classify(&candidate, &policy(), now());

        assert!(!decision.reasons.contains(&ReasonCode::TemporaryCwd));
        assert_eq!(decision.classification, Classification::Keep);
    }

    #[test]
    fn temporary_detection_uses_only_explicit_policy_roots() {
        let mut candidate = candidate("ordinary", 5, 3, 60, "ordinary-session");
        candidate.cwd = Some(PathBuf::from("/tmp/project"));
        let policy =
            ClassifierPolicy::with_temporary_roots(24, vec![PathBuf::from("/var/explicit")]);

        let decision = classify(&candidate, &policy, now());

        assert!(!decision.reasons.contains(&ReasonCode::TemporaryCwd));
        assert!(!decision.reasons.contains(&ReasonCode::FixtureTemporaryCwd));
    }
}
