//! Bounded reads. Every list says whether it was cut: truncation is not
//! absence.

use crate::StoreError;

/// The most items one read may ask for.
pub const MAX_PAGE_LIMIT: u32 = 1000;

/// Which slice of a list to read. Holding one means its limit is in bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRequest {
    limit: u32,
    offset: u32,
}

impl PageRequest {
    /// A page of up to `limit` items, after skipping `offset`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] if `limit` is 0 or over [`MAX_PAGE_LIMIT`].
    pub fn new(limit: u32, offset: u32) -> Result<Self, StoreError> {
        if limit == 0 || limit > MAX_PAGE_LIMIT {
            return Err(StoreError::Invalid(format!(
                "page limit must be 1 to {MAX_PAGE_LIMIT}, got {limit}"
            )));
        }
        Ok(Self { limit, offset })
    }

    /// The most items to return.
    #[must_use]
    pub const fn limit(&self) -> u32 {
        self.limit
    }

    /// How many items to skip.
    #[must_use]
    pub const fn offset(&self) -> u32 {
        self.offset
    }
}

/// A slice of a list.
#[derive(Debug, Clone, PartialEq)]
pub struct Page<T> {
    /// The items.
    pub items: Vec<T>,
    /// True when the list has more items after these.
    pub has_more: bool,
}

impl<T> Page<T> {
    /// Takes the requested slice of an already sorted sequence.
    pub fn from_sorted(sorted: impl IntoIterator<Item = T>, request: PageRequest) -> Self {
        let mut rest = sorted.into_iter().skip(request.offset as usize);
        let items: Vec<T> = rest.by_ref().take(request.limit as usize).collect();
        let has_more = rest.next().is_some();
        Self { items, has_more }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limit_out_of_bounds_is_refused() {
        assert!(PageRequest::new(0, 0).is_err());
        assert!(PageRequest::new(MAX_PAGE_LIMIT + 1, 0).is_err());
        assert!(PageRequest::new(MAX_PAGE_LIMIT, 0).is_ok());
    }

    #[test]
    fn a_page_says_when_it_was_cut() {
        let cut = Page::from_sorted(1..=3, PageRequest::new(2, 0).unwrap());
        assert_eq!((cut.items, cut.has_more), (vec![1, 2], true));
        let whole = Page::from_sorted(1..=3, PageRequest::new(2, 1).unwrap());
        assert_eq!((whole.items, whole.has_more), (vec![2, 3], false));
    }
}
