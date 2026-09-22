//! Conservative automatic-learning policy. Manual revision-bound edits remain
//! available, but ambiguous and inverse rules require explicit confirmation.
pub(crate) struct Proposal {
    pub pairs: Vec<(String, String)>,
    pub message: &'static str,
}

pub(crate) fn risk(from: &str, to: &str, effective: &[(String, String)]) -> bool {
    if from.eq_ignore_ascii_case(to) {
        return false;
    }
    let count = from.chars().count();
    count <= 3
        || matches!(
            from.to_ascii_lowercase().as_str(),
            "eggs" | "like" | "well" | "then" | "that" | "this" | "there" | "right" | "okay"
        )
        || effective
            .iter()
            .any(|(_, written)| written.eq_ignore_ascii_case(from))
}

pub(crate) fn review(
    pairs: &[(String, String)],
    effective: &[(String, String)],
) -> Option<Proposal> {
    let mut proposed = Vec::new();
    let mut needs_review = false;
    let mut inverse = false;
    for (from, to) in pairs {
        let reversing: Vec<_> = effective
            .iter()
            .filter(|(heard, written)| {
                !heard.eq_ignore_ascii_case(written)
                    && written.eq_ignore_ascii_case(from)
                    && heard.eq_ignore_ascii_case(to)
            })
            .collect();
        if !reversing.is_empty() {
            // This is only a suggestion, never silently applied: without audio
            // provenance we cannot prove which interpretation the user intended.
            inverse = true;
            for (heard, _) in reversing {
                proposed.push((heard.clone(), heard.clone()));
            }
        } else {
            needs_review |= risk(from, to, effective);
            proposed.push((from.clone(), to.clone()));
        }
    }
    (inverse || needs_review).then_some(Proposal {
        pairs: proposed,
        message: if inverse {
            "Review before saving: disable a conflicting rule?"
        } else {
            "Review before saving: this rule may change ordinary words."
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pair(a: &str, b: &str) -> (String, String) {
        (a.into(), b.into())
    }
    #[test]
    fn risky_short_common_and_canonical_words_need_confirmation() {
        let rules = vec![pair("考里", "Collie"), pair("vocal code", "VocalCode")];
        for (a, b) in [
            ("in", "linkedin"),
            ("map", "mem"),
            ("ts", "tavus"),
            ("eggs", "x"),
            ("Collie", "考虑"),
            ("呃", "啊"),
        ] {
            assert!(review(&[pair(a, b)], &rules).is_some());
        }
        assert!(review(&[pair("seaquel", "SQL")], &rules).is_none());
        assert!(!risk("in", "in", &rules));
    }
    #[test]
    fn inverse_edit_proposes_disabling_original_rule_not_adding_reverse() {
        let rules = vec![pair("考虑", "Collie")];
        let proposal = review(&[pair("Collie", "考虑")], &rules).unwrap();
        assert_eq!(proposal.pairs, vec![pair("考虑", "考虑")]);
        assert!(proposal.message.contains("conflicting"));
        assert_eq!(rules, vec![pair("考虑", "Collie")]);
    }
    #[test]
    fn risky_batch_defers_even_safe_members() {
        let p = review(&[pair("seaquel", "SQL"), pair("in", "linkedin")], &[]).unwrap();
        assert_eq!(p.pairs.len(), 2);
    }
}
