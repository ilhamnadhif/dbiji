//! SQL language services for DBiji: tokenizer, statement splitter and
//! classifier, formatter, schema-aware autocomplete and analysis,
//! parameters, client commands, outline, highlighting, spell checking, SQL
//! generation, fuzzy matching and optional pre-execution syntax checks.
//!
//! Everything here is pure logic without I/O. Functions are total: they
//! accept arbitrary (incomplete, invalid) text without panicking, because the
//! editor calls them on every keystroke. Positions are byte offsets into the
//! source `&str` and always fall on UTF-8 character boundaries.
//!
//! Lexical rules come from [`dbiji_core::SqlSyntax`] (or a full
//! [`dbiji_core::Dialect`] where quoting, literals or DDL are needed).
//!
//! | Module | Main entry points | Used by |
//! | --- | --- | --- |
//! | [`token`](mod@token) | [`tokenize`](fn@tokenize), [`tokenize_with_delimiter`](fn@tokenize_with_delimiter) | everything |
//! | [`split`](mod@split) | [`split_statements`](fn@split_statements), [`statement_at`](fn@statement_at) | query engine, editor |
//! | [`classify`](mod@classify) | [`classify`](fn@classify), [`split_and_classify`](fn@split_and_classify) | read-only enforcement, confirmations |
//! | [`format`](mod@format) | [`format_sql`](fn@format_sql) | formatter command |
//! | [`complete`](mod@complete) / [`catalog`](mod@catalog) | [`complete`](fn@complete), [`SnapshotCatalog`] | autocomplete |
//! | [`analyze`](mod@analyze) | [`analyze`](fn@analyze), [`object_at`](fn@object_at) | hyperlinks, outline |
//! | [`diagnostics`](mod@diagnostics) | [`diagnostics`](fn@diagnostics), [`script_diagnostics`](fn@script_diagnostics) | error indicators |
//! | [`params`](mod@params) | [`find_parameters`](fn@find_parameters), [`substitute`](fn@substitute) | parameter panel |
//! | [`commands`](mod@commands) | [`parse_client_command`](fn@parse_client_command) | `@set` / `@include` / `@export` |
//! | [`outline`](mod@outline) | [`outline`](fn@outline) | outline view |
//! | [`highlight`](mod@highlight) | [`highlight`](fn@highlight) | syntax colouring |
//! | [`spell`](mod@spell) | [`check_spelling`](fn@check_spelling), [`SpellChecker`] | spell checker |
//! | [`generate`](mod@generate) | [`select_sql`](fn@select_sql), [`rows_insert_sql`](fn@rows_insert_sql) ... | SQL generation |
//! | [`fuzzy`](mod@fuzzy) | [`fuzzy_score`](fn@fuzzy_score), [`fuzzy_indices`](fn@fuzzy_indices) | command palette |
//! | [`position`](mod@position) | [`line_col`](fn@line_col), [`char_pos_to_byte`](fn@char_pos_to_byte) | error positions |
//! | [`syntax_check`](mod@syntax_check) | [`syntax_check`](fn@syntax_check) | pre-execution warnings |

#![warn(missing_docs)]

pub mod analyze;
pub mod catalog;
pub mod classify;
pub mod commands;
pub mod complete;
pub mod diagnostics;
pub mod format;
pub mod fuzzy;
pub mod generate;
pub mod highlight;
pub mod outline;
pub mod params;
pub mod position;
pub mod spell;
pub mod split;
pub mod syntax_check;
pub mod token;

mod lexicon;

#[cfg(test)]
mod bench_tests;
#[cfg(test)]
mod chaos_tests;
#[cfg(test)]
mod testutil;

use serde::{Deserialize, Serialize};

/// Letter case used when emitting keywords (formatter, autocomplete).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum KeywordCase {
    /// `SELECT`
    #[default]
    Upper,
    /// `select`
    Lower,
    /// Keep the case the keyword was written in (or the case of the source
    /// list when the keyword is generated).
    Preserve,
}

impl KeywordCase {
    /// Apply the case to `word`.
    pub fn apply(self, word: &str) -> String {
        match self {
            KeywordCase::Upper => word.to_uppercase(),
            KeywordCase::Lower => word.to_lowercase(),
            KeywordCase::Preserve => word.to_string(),
        }
    }
}

pub use analyze::{
    CteInfo, ObjectReference, RefRole, StatementAnalysis, SubqueryInfo, TableReference, analyze,
    object_at,
};
pub use catalog::{CatalogTable, CompletionCatalog, SnapshotCatalog};
pub use classify::{
    Danger, QualifiedName, StatementInfo, StatementKind, TxCommand, classify, split_and_classify,
};
pub use commands::{ClientCommand, parse_client_command};
pub use complete::{CompletionItem, CompletionKind, CompletionOptions, CompletionResult, complete};
pub use diagnostics::{Diagnostic, Severity, diagnostics, script_diagnostics};
pub use format::{FormatOptions, IndentStyle, format_sql};
pub use fuzzy::{fuzzy_indices, fuzzy_score};
pub use generate::{
    GenerateOptions, ddl_sql, delete_sql, insert_sql, rows_delete_sql, rows_insert_sql,
    rows_update_sql, select_sql, update_sql, upsert_sql,
};
pub use highlight::{HighlightClass, highlight};
pub use outline::{OutlineItem, OutlineKind, outline};
pub use params::{ParamKind, ParamOptions, ParamRef, find_parameters, parameter_names, substitute};
pub use position::{char_pos_to_byte, line_col, offset_of_line};
pub use spell::{SpellChecker, SpellIssue, check_spelling};
pub use split::{BlankLineMode, SplitOptions, StatementSpan, split_statements, statement_at};
pub use syntax_check::{SyntaxIssue, syntax_check};
pub use token::{Token, TokenKind, tokenize, tokenize_with_delimiter};

#[cfg(test)]
mod lib_tests {
    use super::*;

    #[test]
    fn keyword_case() {
        assert_eq!(KeywordCase::Upper.apply("select"), "SELECT");
        assert_eq!(KeywordCase::Lower.apply("SeLeCt"), "select");
        assert_eq!(KeywordCase::Preserve.apply("SeLeCt"), "SeLeCt");
    }
}
