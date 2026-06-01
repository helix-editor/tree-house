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

static EXTENDS_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r";+\s*extends\s*").unwrap());

/// reads a query by invoking `read_query_text`, handles any `inherits` directives.
///
/// if you need to also handle `extends` directives, you can use the [`read_query_extends`]
/// function.
pub fn read_query(language: &str, mut read_query_text: impl FnMut(&str) -> String) -> String {
    fn read_query_impl(language: &str, read_query_text: &mut impl FnMut(&str) -> String) -> String {
        let query = read_query_text(language);

        // replaces all "; inherits <language>(,<language>)*" with the queries of the given language(s)
        INHERITS_REGEX
            .replace_all(&query, |captures: &regex::Captures| {
                captures[1]
                    .split(',')
                    .fold(String::new(), |mut output, language| {
                        // `write!` to a String cannot fail.
                        write!(
                            output,
                            "\n{}\n",
                            read_query_impl(language, &mut *read_query_text)
                        )
                        .unwrap();
                        output
                    })
            })
            .into_owned()
    }
    read_query_impl(language, &mut read_query_text)
}

/// gets a list of queries in priority order from highest to lowest by invoking
/// `read_lang_queries`, handles any `extends` and `inherits` directives.
///
/// this function is very similar to [`read_query`], however it also handles `extends`
/// directives in addition to `inherits`.
pub fn read_query_extends<I>(language: &str, mut read_lang_queries: impl FnMut(&str) -> I) -> String
where
    I: Iterator<Item = String>,
{
    fn read_query_impl<I>(read_lang_queries: &mut impl FnMut(&str) -> I, mut queries: I) -> String
    where
        I: Iterator<Item = String>,
    {
        let Some(mut query) = queries.next() else {
            return String::new();
        };

        // replace all "; extends" with the queries of the current language, one precedence level up
        if let Some(m) = EXTENDS_REGEX.find(&query) {
            let q = read_query_impl(read_lang_queries, queries);
            query.replace_range(m.range(), &q);
        }

        // replaces all "; inherits <language>(,<language>)*" with the queries of the given language(s)
        INHERITS_REGEX
            .replace_all(&query, |captures: &regex::Captures| {
                captures[1]
                    .split(',')
                    .fold(String::new(), |mut output, language| {
                        let queries = read_lang_queries(language);
                        // `write!` to a String cannot fail.
                        write!(
                            output,
                            "\n{}\n",
                            read_query_impl(&mut *read_lang_queries, queries)
                        )
                        .unwrap();
                        output
                    })
            })
            .into_owned()
    }

    let queries = read_lang_queries(language);
    read_query_impl(&mut read_lang_queries, queries)
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
