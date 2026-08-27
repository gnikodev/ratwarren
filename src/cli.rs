pub enum Invocation {
    Run { name: Option<String> },
    SetPassword { name: String },
    Help,
    BadUsage(String),
}

pub const USAGE: &str = "usage: ratwarren [<connection>]\n       ratwarren --set-password <connection>\n       ratwarren --help";

pub fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Invocation {
    args.next(); // skip argv[0]
    match args.next() {
        None => Invocation::Run { name: None },
        Some(a) if a == "--help" || a == "-h" => Invocation::Help,
        Some(a) if a == "--set-password" => match args.next() {
            Some(name) if args.next().is_none() => Invocation::SetPassword { name },
            Some(_) => {
                Invocation::BadUsage("--set-password takes exactly one connection name".to_string())
            }
            None => Invocation::BadUsage("--set-password requires a connection name".to_string()),
        },
        Some(a) if a.starts_with('-') => Invocation::BadUsage(format!("unknown flag: {a}")),
        Some(name) => {
            if args.next().is_some() {
                Invocation::BadUsage("too many arguments".to_string())
            } else {
                Invocation::Run { name: Some(name) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> impl Iterator<Item = String> {
        items
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn no_args_runs_with_no_connection_name() {
        assert!(matches!(
            parse_args(args(&["ratwarren"])),
            Invocation::Run { name: None }
        ));
    }

    #[test]
    fn one_plain_arg_runs_with_that_connection_name() {
        match parse_args(args(&["ratwarren", "prod"])) {
            Invocation::Run { name: Some(n) } => assert_eq!(n, "prod"),
            other => panic!(
                "expected Run{{name: Some(_)}}, got a different variant: {}",
                matches_desc(&other)
            ),
        }
    }

    #[test]
    fn help_flag_is_recognized_long_and_short() {
        assert!(matches!(
            parse_args(args(&["ratwarren", "--help"])),
            Invocation::Help
        ));
        assert!(matches!(
            parse_args(args(&["ratwarren", "-h"])),
            Invocation::Help
        ));
    }

    #[test]
    fn set_password_with_a_name_is_recognized() {
        match parse_args(args(&["ratwarren", "--set-password", "foo"])) {
            Invocation::SetPassword { name } => assert_eq!(name, "foo"),
            other => panic!(
                "expected SetPassword, got a different variant: {}",
                matches_desc(&other)
            ),
        }
    }

    #[test]
    fn set_password_alone_is_bad_usage() {
        assert!(matches!(
            parse_args(args(&["ratwarren", "--set-password"])),
            Invocation::BadUsage(_)
        ));
    }

    #[test]
    fn set_password_with_two_names_is_bad_usage() {
        assert!(matches!(
            parse_args(args(&["ratwarren", "--set-password", "foo", "bar"])),
            Invocation::BadUsage(_)
        ));
    }

    #[test]
    fn unknown_flag_is_bad_usage() {
        assert!(matches!(
            parse_args(args(&["ratwarren", "--bogus"])),
            Invocation::BadUsage(_)
        ));
    }

    #[test]
    fn two_plain_args_is_bad_usage() {
        assert!(matches!(
            parse_args(args(&["ratwarren", "foo", "bar"])),
            Invocation::BadUsage(_)
        ));
    }

    #[test]
    fn empty_string_connection_name_is_treated_as_a_literal_valid_name() {
        // Not a special "no name given" case -- `args.next()` returns
        // `Some("")`, which is neither `--help`/`-h` nor starts with `-`, so
        // it falls into the plain-name branch as-is.
        match parse_args(args(&["ratwarren", ""])) {
            Invocation::Run { name: Some(n) } => assert_eq!(n, ""),
            other => panic!(
                "expected Run{{name: Some(\"\")}}, got a different variant: {}",
                matches_desc(&other)
            ),
        }
    }

    #[test]
    fn set_password_with_a_flag_shaped_name_is_treated_as_a_literal_connection_name() {
        // `--set-password`'s argument is taken unconditionally as the name --
        // it is never itself re-interpreted as `--help` or another flag.
        match parse_args(args(&["ratwarren", "--set-password", "--help"])) {
            Invocation::SetPassword { name } => assert_eq!(name, "--help"),
            other => panic!(
                "expected SetPassword{{name: \"--help\"}}, got a different variant: {}",
                matches_desc(&other)
            ),
        }
    }

    #[test]
    fn a_flag_appearing_after_a_positional_name_is_not_reinterpreted_as_set_password() {
        // Argument order matters: `--set-password` is only recognized as the
        // very first argument. Once a plain name has been consumed first,
        // a later `--set-password` is just an unexpected extra argument.
        assert!(matches!(
            parse_args(args(&["ratwarren", "foo", "--set-password"])),
            Invocation::BadUsage(_)
        ));
    }

    fn matches_desc(inv: &Invocation) -> &'static str {
        match inv {
            Invocation::Run { .. } => "Run",
            Invocation::SetPassword { .. } => "SetPassword",
            Invocation::Help => "Help",
            Invocation::BadUsage(_) => "BadUsage",
        }
    }
}
