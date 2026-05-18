//! Email (.eml / .mbox) extractor via `mail-parser`. Subject becomes the
//! top-level heading; From/To/Date go into YAML frontmatter; HTML bodies
//! are run through the HTML extractor.

#![allow(dead_code)]
