use once_cell::sync::Lazy;
use regex::Regex;
use tree_sitter::{query, Grammar};

use crate::highlighter::{Highlight, HighlightQuery};
use crate::injections_query::{InjectionLanguageMarker, InjectionsQuery};
use crate::Language;

use std::fmt::Write;

#[derive(Debug)]
pub struct LanguageConfig {
    pub grammar: Grammar,
    pub highlight_query: HighlightQuery,
    pub injection_query: InjectionsQuery,
}

impl LanguageConfig {
    pub fn new(
        grammar: Grammar,
        highlight_query_text: &str,
        injection_query_text: &str,
        local_query_text: &str,
    ) -> Result<Self, query::ParseError> {
        // NOTE: the injection queries are parsed first since the local query is passed as-is
        // to `Query::new` in `InjectionsQuery::new`. This ensures that the more readable error
        // bubbles up first if the locals queries have an issue.
        let injection_query =
            InjectionsQuery::new(grammar, injection_query_text, local_query_text)?;
        let highlight_query = HighlightQuery::new(grammar, highlight_query_text, local_query_text)?;

        Ok(Self {
            grammar,
            highlight_query,
            injection_query,
        })
    }

    pub fn configure(&self, mut f: impl FnMut(&str) -> Option<Highlight>) {
        self.highlight_query.configure(&mut f);
        self.injection_query.configure(&mut f);
    }
}

static INHERITS_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r";+\s*inherits\s*:?\s*([a-z_,()-]+)\s*").unwrap());

/// reads a query by invoking `read_query_text`, handles any `inherits` directives
pub fn read_query(language: &str, mut read_query_text: impl FnMut(&str) -> String) -> String {
    fn read_query_impl(
        language: &str,
        read_query_text: &mut impl FnMut(&str) -> String,
        // The chain of languages currently being expanded, used to break cyclic
        // `; inherits:` directives (e.g. `a` inherits `b` and `b` inherits `a`).
        chain: &mut Vec<String>,
    ) -> String {
        if chain.iter().any(|ancestor| ancestor == language) {
            return String::new();
        }
        chain.push(language.to_string());
        let query = read_query_text(language);

        // replaces all "; inherits <language>(,<language>)*" with the queries of the given language(s)
        let result = INHERITS_REGEX
            .replace_all(&query, |captures: &regex::Captures| {
                captures[1]
                    .split(',')
                    .fold(String::new(), |mut output, language| {
                        // `write!` to a String cannot fail.
                        write!(
                            output,
                            "\n{}\n",
                            read_query_impl(language, &mut *read_query_text, chain)
                        )
                        .unwrap();
                        output
                    })
            })
            .into_owned();
        chain.pop();
        result
    }
    read_query_impl(language, &mut read_query_text, &mut Vec::new())
}

pub trait LanguageLoader {
    fn language_for_marker(&self, marker: InjectionLanguageMarker) -> Option<Language>;
    fn get_config(&self, lang: Language) -> Option<&LanguageConfig>;
}

impl<T> LanguageLoader for &'_ T
where
    T: LanguageLoader,
{
    fn language_for_marker(&self, marker: InjectionLanguageMarker) -> Option<Language> {
        T::language_for_marker(self, marker)
    }

    fn get_config(&self, lang: Language) -> Option<&LanguageConfig> {
        T::get_config(self, lang)
    }
}

#[cfg(test)]
mod tests {
    use super::read_query;

    #[test]
    fn read_query_breaks_inherits_cycle() {
        // `a` inherits `b` and `b` inherits `a`: resolution must terminate rather than
        // recurse until the stack overflows, and still include both queries' own patterns.
        let result = read_query("a", |lang| match lang {
            "a" => "; inherits: b\n(a_pattern) @a".to_string(),
            "b" => "; inherits: a\n(b_pattern) @b".to_string(),
            _ => String::new(),
        });
        assert!(result.contains("@a"), "missing a's own patterns: {result:?}");
        assert!(result.contains("@b"), "missing b's own patterns: {result:?}");
    }

    #[test]
    fn read_query_breaks_self_inherit() {
        let result = read_query("a", |lang| match lang {
            "a" => "; inherits: a\n(a_pattern) @a".to_string(),
            _ => String::new(),
        });
        assert!(result.contains("@a"), "missing a's own patterns: {result:?}");
    }
}
