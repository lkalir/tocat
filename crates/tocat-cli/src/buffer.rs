//! buffer.rs - page-aligned copy buffers.

/// One page, aligned to a page.
#[repr(align(4096))]
#[derive(Clone, Copy)]
pub struct Page(#[allow(unused)] [u8; Buffer::PAGE]);

const _: () = {
    assert!(size_of::<Page>() == Buffer::PAGE);
    assert!(align_of::<Page>() == Buffer::PAGE);
};

/// A page-aligned byte buffer.
pub struct Buffer {
    pages: Vec<Page>,
    len: usize,
}

impl Buffer {
    const PAGE: usize = 4096;

    /// Allocate at least `len` bytes, page-aligned and zeroed.
    ///
    /// Rounds up to a whole number of pages; the extra is never exposed, so a
    /// caller asking for 1000 bytes still sees a 1000-byte slice.
    #[must_use]
    pub fn new(len: usize) -> Self {
        let pages = len.div_ceil(Self::PAGE).max(1);

        Self {
            pages: vec![Page([0; Self::PAGE]); pages],
            len,
        }
    }

    fn as_bytes(pages: &[Page]) -> &[u8] {
        // SAFETY: `Page` is `#[repr(align(4096))]` around `[u8; 4096]`, so it
        // has no padding and no invalid bit patterns; a slice of them is a
        // contiguous run of `len * 4096` initialised bytes.
        unsafe {
            std::slice::from_raw_parts(pages.as_ptr().cast::<u8>(), std::mem::size_of_val(pages))
        }
    }

    fn as_bytes_mut(pages: &mut [Page]) -> &mut [u8] {
        let len = std::mem::size_of_val(pages);
        // SAFETY: as above, and the borrow is exclusive.
        unsafe { std::slice::from_raw_parts_mut(pages.as_mut_ptr().cast::<u8>(), len) }
    }
}

impl std::ops::Deref for Buffer {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY-free: `Page` is a plain byte array, so the vector's storage is
        // already a contiguous run of bytes.
        &Self::as_bytes(&self.pages)[..self.len]
    }
}

impl std::ops::DerefMut for Buffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut Self::as_bytes_mut(&mut self.pages)[..self.len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_page_aligned() {
        for len in [1, 4095, 4096, 4097, 256 * 1024] {
            let buf = Buffer::new(len);
            assert_eq!(buf.as_ptr() as usize % Buffer::PAGE, 0, "len {len}");
            assert_eq!(buf.len(), len, "len {len}");
        }
    }

    #[test]
    fn is_zeroed_and_writable() {
        let mut buf = Buffer::new(8192);
        assert!(buf.iter().all(|&b| b == 0));

        buf[8191] = 7;
        assert_eq!(buf[8191], 7);
    }
}
