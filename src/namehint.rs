//! "did you mean" for names that only exist at runtime.
//!
//! Shellsong validates keyword arguments while parsing and gets good
//! errors for free, but names the user invented (saved eq curves, saved
//! queues) are not in any static table, so those commands resolve at
//! apply time instead. That is the right place for it; the cost is that
//! the error has to be built by hand, and the first two that needed it
//! each grew their own. This is the shared one.

/// Jaro-Winkler rather than an edit distance, matching shellsong's own
/// `did_you_mean`: a real name is often much longer than what was typed
/// ("my bass" against "my bass curve"), where an edit-distance budget
/// scaled to the typed length refuses every useful answer. Prefix
/// weighting also suits a half-typed name.
const SUGGEST_FLOOR: u32 = 820;

/// How much of the known-names list an error message will carry before
/// it stops being readable in a notification. Past this the list is cut
/// and the remainder counted, rather than printing a hundred names into
/// a one-line toast.
const MAX_LISTED: usize = 40;

/// Closest name in `among` to `typed`, or `None` if nothing is near.
/// Case-insensitive, since every caller's names are too.
pub fn closest<'a>(typed: &str, among: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let lower = typed.to_lowercase();
    let mut hits: Vec<(u32, &'a str)> = among
        .into_iter()
        .map(|c| {
            (
                (strsim::jaro_winkler(&lower, &c.to_lowercase()) * 1000.0) as u32,
                c,
            )
        })
        .filter(|(score, _)| *score >= SUGGEST_FLOOR)
        .collect();
    // closest, then shortest, then alphabetical: deterministic all the
    // way down, so the same typo always suggests the same name.
    hits.sort_by_key(|(score, c)| (std::cmp::Reverse(*score), c.len(), *c));
    hits.first().map(|(_, c)| *c)
}

/// Why `typed` did not resolve, phrased for whoever typed it: the
/// closest known name if one is near, the list of what exists if not,
/// and plain "none saved yet" when there is nothing to list (which is
/// the likeliest reason a name missed in the first place).
///
/// `kind` names the thing in the singular, lowercase: "eq preset",
/// "saved queue".
pub fn unknown<'a>(kind: &str, typed: &str, among: impl IntoIterator<Item = &'a str>) -> String {
    let names: Vec<&str> = among.into_iter().collect();
    if let Some(near) = closest(typed, names.iter().copied()) {
        return format!("no {kind} '{typed}'; did you mean '{near}'?");
    }
    if names.is_empty() {
        return format!("no {kind} '{typed}'; none saved yet");
    }
    let shown = names.len().min(MAX_LISTED);
    let mut list = names[..shown].join(", ");
    if shown < names.len() {
        list.push_str(&format!(" (+{} more)", names.len() - shown));
    }
    format!("no {kind} '{typed}'; have: {list}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::textfit::width;

    #[test]
    fn a_typo_suggests_the_closest_name() {
        assert_eq!(closest("rok", ["flat", "rock", "jazz"]), Some("rock"));
    }

    #[test]
    fn a_half_typed_name_still_suggests_the_long_one() {
        // the reason this is jaro-winkler: "my bass" is six edits from
        // "my bass curve", which any length-scaled budget would refuse.
        assert_eq!(
            closest("my bass", ["my bass curve", "treble lift"]),
            Some("my bass curve")
        );
    }

    #[test]
    fn nothing_near_suggests_nothing() {
        assert_eq!(closest("disco", ["flat", "rock"]), None);
    }

    #[test]
    fn matching_ignores_case() {
        assert_eq!(closest("ROCK", ["rock"]), Some("rock"));
        assert_eq!(closest("rock", ["ROCK"]), Some("ROCK"));
    }

    /// Same typo, same answer, whatever order the names arrive in.
    #[test]
    fn the_suggestion_does_not_depend_on_input_order() {
        let a = closest("rok", ["rock", "rocky", "rockabilly"]);
        let b = closest("rok", ["rockabilly", "rocky", "rock"]);
        assert_eq!(a, b);
        assert_eq!(a, Some("rock"), "closest, then shortest");
    }

    #[test]
    fn an_unknown_name_with_a_near_miss_suggests_it() {
        let msg = unknown("eq preset", "rok", ["flat", "rock"]);
        assert_eq!(msg, "no eq preset 'rok'; did you mean 'rock'?");
    }

    #[test]
    fn an_unknown_name_with_nothing_near_lists_what_there_is() {
        let msg = unknown("eq preset", "disco", ["flat", "rock"]);
        assert_eq!(msg, "no eq preset 'disco'; have: flat, rock");
    }

    /// The case a bare "have: " would have read as a bug: the user has
    /// saved nothing, which is almost certainly why the name missed.
    #[test]
    fn an_empty_list_says_so_instead_of_listing_nothing() {
        let msg = unknown("saved queue", "roadtrip", std::iter::empty());
        assert_eq!(msg, "no saved queue 'roadtrip'; none saved yet");
    }

    #[test]
    fn a_long_list_is_cut_and_counted_rather_than_dumped() {
        let names: Vec<String> = (0..60).map(|i| format!("queue{i:02}")).collect();
        let msg = unknown("saved queue", "zzz", names.iter().map(String::as_str));
        assert!(msg.contains("(+20 more)"), "{msg}");
        assert!(msg.contains("queue00"), "{msg}");
        assert!(!msg.contains("queue40"), "{msg}");
    }

    /// A name can hold wide glyphs, so the message is measured in cells
    /// like every other string this app prints.
    #[test]
    fn the_message_is_measured_in_cells_not_chars() {
        let msg = unknown("saved queue", "x", ["日本語"]);
        assert!(
            width(&msg) > msg.chars().count(),
            "wide glyphs should cost more cells than chars: {msg}"
        );
    }
}
