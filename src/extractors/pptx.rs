//! PPTX extractor — `zip` + `quick-xml` over `ppt/slides/slide*.xml` and
//! `ppt/notesSlides/notesSlide*.xml`.
//!
//! Slide N becomes `## Slide N — <title>`; bullets become a list; speaker
//! notes become a blockquote.

#![allow(dead_code)]
