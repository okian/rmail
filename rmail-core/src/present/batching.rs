//! Splitting a best-first [`PresentedResult`] list into streaming-ready
//! pages (prd.md, Stage 6: "results flush best-first in score-ordered
//! batches so the top result paints in <30 ms even while lower ranks are
//! still being reranked").
//!
//! # A thin wrapper, deliberately
//!
//! [`batch`] does not sort, rescore, or otherwise touch what order
//! [`Presenter::present`](super::Presenter::present) already decided — it
//! only cuts an already-ordered slice into pages. That thinness is the
//! point: task 33's `SearchService.Search` streams one page per `send()`
//! call, and the contract a client can build an incremental renderer against
//! is exactly "whatever order the pages arrive in is the order results
//! should paint in, and no result ever arrives twice." A batching step that
//! did its own re-sorting per page (or fetched pages from the database with
//! separate `LIMIT`/`OFFSET` queries against a possibly-changing candidate
//! set) could violate either half of that; slicing an in-memory `Vec` that
//! is already final cannot.

use super::PresentedResult;

/// prd.md gives no explicit page size for Stage 6's streaming batches — this
/// is a reasonable default a caller (task 33) can override:
/// small enough that the very first batch reaches a client fast (the whole
/// point of streaming best-first), large enough that most single-digit
/// result sets fit in one batch and do not pay a second round trip for no
/// reason.
pub const DEFAULT_BATCH_SIZE: usize = 10;

/// Split `results` — already best-first, as
/// [`Presenter::present`](super::Presenter::present) returns it — into pages
/// of at most `batch_size` each, preserving order and duplicating nothing.
///
/// `batch_size` of `0` is clamped to `1` rather than producing an empty (and
/// therefore infinite-looking, to a caller that loops "until an empty page")
/// stream of pages for a non-empty `results`.
///
/// Concatenating every returned page, in order, reproduces `results` exactly
/// — this is the "no duplicates across batch boundaries, non-increasing
/// score between batches" contract task 33's streaming depends on, and it
/// holds structurally (a `chunks` partition can neither drop, reorder, nor
/// repeat an element) rather than needing to be separately proven for every
/// input; see `tests` for the check that pins it anyway, since a future
/// rewrite of this function should not be able to quietly break it.
#[must_use]
pub fn batch(results: &[PresentedResult], batch_size: usize) -> Vec<Vec<PresentedResult>> {
    results
        .chunks(batch_size.max(1))
        .map(<[PresentedResult]>::to_vec)
        .collect()
}

#[cfg(test)]
mod tests;
