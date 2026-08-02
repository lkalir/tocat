//! normalize.rs: one spelling rule for every identifier tocat matches.
//!
//! Schemes, endpoint options, plugin names, plugin option keys and enum values
//! are all matched the same way: case is ignored and dashes and underscores are
//! noise. `max-connections`, `max_connections` and `MaxConnections` are one
//! option, and a user who guesses wrong about a convention is right anyway.
//!
//! The rule is matching only. A normalized string is never stored, forwarded or
//! displayed: [`canonical`] answers *which declared spelling was meant*, and
//! the caller then uses that declared spelling. Free-form values (paths,
//! labels, instance aliases, commands) never come near this module.

/// Normalize an identifier to a consistent form: lowercase, with no dashes or
/// underscores.
#[must_use]
pub fn normalize(item: &str) -> String {
    item.chars()
        .filter(|&c| c != '-' && c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Which of `declared` the user meant by `candidate`, if exactly one.
///
/// An exact match wins outright, so the common case costs one comparison and
/// cannot regress. Otherwise a candidate matches a declaration when the two
/// normalize alike, and only an unambiguous match counts.
///
/// `None` means the caller should pass `candidate` through untouched rather
/// than guess. That is what keeps a plugin's own `#[serde(alias)]` spellings
/// working, since serde does not report aliases alongside the names it
/// declares, and what leaves an unknown identifier to be reported by whoever
/// owns the vocabulary.
#[must_use]
pub fn canonical<'a>(candidate: &str, declared: &[&'a str]) -> Option<&'a str> {
    if let Some(exact) = declared.iter().find(|d| **d == candidate) {
        return Some(exact);
    }

    let wanted = normalize(candidate);
    let mut hits = declared.iter().filter(|d| normalize(d) == wanted);
    let first = *hits.next()?;

    hits.next().is_none().then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separators_and_case_are_noise() {
        assert_eq!(normalize("Max-Connections"), "maxconnections");
        assert_eq!(normalize("max_connections"), "maxconnections");
        assert_eq!(normalize("MAXCONNECTIONS"), "maxconnections");
    }

    #[test]
    fn exact_match_wins() {
        assert_eq!(
            canonical("max-conn", &["max-conn", "maxconn"]),
            Some("max-conn")
        );
    }

    #[test]
    fn spelling_is_recovered() {
        assert_eq!(
            canonical("Raw_Binary", &["hex", "raw-binary"]),
            Some("raw-binary")
        );
    }

    #[test]
    fn ambiguity_and_strangers_are_left_alone() {
        assert_eq!(canonical("maxconn", &["max-conn", "max_conn"]), None);
        assert_eq!(canonical("nonsense", &["hex", "raw-binary"]), None);
    }
}
