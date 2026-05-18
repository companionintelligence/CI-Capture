//! DOCX extractor via `docx-rs`.
//!
//! Walks the body, maps Word styles (Heading 1..6) to MD headings, lists
//! to MD lists, tables to MD tables. Comments + tracked changes are
//! captured as optional sidebar content.

#![allow(dead_code)]
