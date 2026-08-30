//! Framework-independent parsing of launcher and single-instance intents.

/// One semantic action requested through desktop launcher arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliIntent {
    ToggleDictation,
    ToggleQa,
    CancelDictation,
}

/// Return the first recognised intent and ignore all unrelated launcher args.
///
/// `args[0]` is always treated as the executable path, even if it happens to
/// look like a supported flag.
pub fn parse_cli_intent<S: AsRef<str>>(args: &[S]) -> Option<CliIntent> {
    for arg in args.iter().skip(1) {
        match arg.as_ref() {
            "--toggle-dictation" => return Some(CliIntent::ToggleDictation),
            "--toggle-qa" => return Some(CliIntent::ToggleQa),
            "--cancel-dictation" | "--cancel" => return Some(CliIntent::CancelDictation),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_argv0_only_has_no_intent() {
        let empty: Vec<&str> = vec![];
        assert_eq!(parse_cli_intent(&empty), None);
        assert_eq!(parse_cli_intent(&["openless"]), None);
    }

    #[test]
    fn recognises_every_supported_intent_and_cancel_alias() {
        assert_eq!(
            parse_cli_intent(&["openless", "--toggle-dictation"]),
            Some(CliIntent::ToggleDictation)
        );
        assert_eq!(
            parse_cli_intent(&["openless", "--toggle-qa"]),
            Some(CliIntent::ToggleQa)
        );
        assert_eq!(
            parse_cli_intent(&["openless", "--cancel-dictation"]),
            Some(CliIntent::CancelDictation)
        );
        assert_eq!(
            parse_cli_intent(&["openless", "--cancel"]),
            Some(CliIntent::CancelDictation)
        );
    }

    #[test]
    fn ignores_unknown_args_and_returns_first_match() {
        assert_eq!(
            parse_cli_intent(&["openless", "--unknown", "/some/path"]),
            None
        );
        assert_eq!(
            parse_cli_intent(&[
                "openless",
                "/some/path",
                "--toggle-dictation",
                "--toggle-qa",
            ]),
            Some(CliIntent::ToggleDictation)
        );
    }

    #[test]
    fn never_treats_argv0_as_an_intent() {
        assert_eq!(parse_cli_intent(&["--toggle-dictation"]), None);
    }
}
