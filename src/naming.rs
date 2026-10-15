use crate::zoom::Meeting;
use chrono::Local;
use std::collections::HashSet;

const MAX_STEM_CHARS: usize = 150;

/// Plans a unique, Windows-safe file name for every file of every meeting.
/// `meetings` must be in a stable order so names stay the same between runs.
pub fn plan_names(meetings: &[Meeting]) -> Vec<Vec<String>> {
    let mut used = HashSet::new();
    meetings
        .iter()
        .map(|m| {
            let topic = sanitize(&m.topic);
            let bases: Vec<String> = m
                .files
                .iter()
                .map(|f| format!("{topic} - {}", f.recording_start.with_timezone(&Local).format("%Y-%m-%d %Hh%M")))
                .collect();

            m.files
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    let mut base = bases[i].clone();
                    if bases.iter().filter(|b| **b == base).count() > 1 && !f.recording_type.is_empty() {
                        base = format!("{base} ({})", f.recording_type.replace('_', " "));
                    }
                    let mut name = format!("{base}.mp4");
                    let mut n = 2;
                    // Windows file names are case-insensitive.
                    while !used.insert(name.to_lowercase()) {
                        name = format!("{base} ({n}).mp4");
                        n += 1;
                    }
                    name
                })
                .collect()
        })
        .collect()
}

pub fn sanitize(s: &str) -> String {
    let replaced: String = s
        .chars()
        .map(|c| match c {
            ':' | '/' | '\\' | '|' => '-',
            '<' | '>' | '"' | '?' | '*' => '_',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let collapsed = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated: String = collapsed.chars().take(MAX_STEM_CHARS).collect();
    let trimmed = truncated.trim_end_matches(['.', ' ']).trim_start();

    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1",
        "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if trimmed.is_empty() {
        "Meeting".to_string()
    } else if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(trimmed)) {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zoom::RecordingFile;
    use chrono::{TimeZone, Utc};

    fn file(id: &str, kind: &str, minute: u32) -> RecordingFile {
        RecordingFile {
            id: id.into(),
            recording_type: kind.into(),
            recording_start: Utc.with_ymd_and_hms(2026, 1, 2, 6, minute, 0).unwrap(),
            download_url: "https://example.com".into(),
            size: 1,
        }
    }

    fn meeting(uuid: &str, files: Vec<RecordingFile>) -> Meeting {
        Meeting {
            uuid: uuid.into(),
            topic: "Daf Ayomi: Rav/Maknouz?".into(),
            start_time: Utc.with_ymd_and_hms(2026, 1, 2, 6, 6, 0).unwrap(),
            duration_min: 60,
            files,
        }
    }

    #[test]
    fn names_are_unique_and_safe() {
        let meetings = vec![
            meeting("a", vec![file("1", "shared_screen_with_speaker_view", 6), file("2", "shared_screen_with_speaker_view", 9)]),
            meeting("b", vec![file("3", "speaker_view", 9), file("4", "gallery_view", 9)]),
        ];
        let names: Vec<String> = plan_names(&meetings).into_iter().flatten().collect();
        let unique: HashSet<_> = names.iter().map(|n| n.to_lowercase()).collect();
        assert_eq!(unique.len(), names.len());
        for n in &names {
            assert!(!n.contains([':', '/', '?', '\\']), "{n}");
            assert!(n.starts_with("Daf Ayomi- Rav-Maknouz_ - "), "{n}");
        }
    }

    #[test]
    fn sanitize_edge_cases() {
        assert_eq!(sanitize("  con "), "_con");
        assert_eq!(sanitize("..."), "Meeting");
        assert_eq!(sanitize("a\tb   c."), "a b c");
    }
}
