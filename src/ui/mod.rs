pub mod editor;
pub mod grid;
pub mod pages;
pub mod picker;
pub mod tree;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(pub u64);

#[derive(Debug, Clone, Default, PartialEq)]
pub enum Load<T> {
    #[default]
    NotLoaded,
    Loading {
        id: RequestId,
    },
    Loaded(T),
    Failed {
        message: String,
    },
}

pub fn error_chain(err: &dyn std::error::Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(err) = source {
        message.push_str(": ");
        message.push_str(&err.to_string());
        source = err.source();
    }
    message
}

pub fn first_line(s: &str) -> &str {
    s.split('\n').next().unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Leaf;
    impl std::fmt::Display for Leaf {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "leaf")
        }
    }
    impl std::error::Error for Leaf {}

    #[derive(Debug)]
    struct Wrapper(Leaf);
    impl std::fmt::Display for Wrapper {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "wrapper")
        }
    }
    impl std::error::Error for Wrapper {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn error_chain_joins_all_sources() {
        let err = Wrapper(Leaf);
        assert_eq!(error_chain(&err), "wrapper: leaf");
    }

    #[test]
    fn error_chain_without_source_is_just_the_message() {
        let err = Leaf;
        assert_eq!(error_chain(&err), "leaf");
    }

    #[test]
    fn first_line_splits_on_newline() {
        assert_eq!(first_line("a\nb\nc"), "a");
    }

    #[test]
    fn first_line_returns_whole_string_without_newline() {
        assert_eq!(first_line("just one line"), "just one line");
    }
}
