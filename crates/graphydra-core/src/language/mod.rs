//! GHQL (the GrapHydra query language): lexer, parser and AST.

pub mod ast;
pub(crate) mod lexer;
pub mod parser;

pub use parser::{parse_script, parse_statement};
