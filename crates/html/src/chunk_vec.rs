//! A growable list stored in fixed-size chunks.
//!
//! `Vec` grows by doubling: it allocates a buffer twice the size, copies, then frees the old
//! one, so for a moment both are alive and peak memory is about 3x the data. A chunked list
//! never copies and wastes at most one partly filled chunk. Engine-wide lists with hundreds of
//! thousands of entries (DOM nodes, text fragments) use this to hold the 60 MB budget.

use std::ops::{Index, IndexMut};

const CHUNK: usize = 4096;

pub struct ChunkVec<T> {
    chunks: Vec<Vec<T>>,
    len: usize,
}

impl<T> ChunkVec<T> {
    pub const fn new() -> ChunkVec<T> {
        ChunkVec {
            chunks: Vec::new(),
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(&mut self, value: T) {
        match self.chunks.last_mut() {
            Some(last) if last.len() < CHUNK => last.push(value),
            _ => {
                let mut chunk = Vec::with_capacity(CHUNK);
                chunk.push(value);
                self.chunks.push(chunk);
            }
        }
        self.len += 1;
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.chunks.get(index / CHUNK)?.get(index % CHUNK)
    }

    pub fn last_mut(&mut self) -> Option<&mut T> {
        self.chunks.last_mut()?.last_mut()
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        self.chunks.iter().flatten()
    }

    /// Release the unused tail of the last chunk.
    pub fn shrink_to_fit(&mut self) {
        if let Some(last) = self.chunks.last_mut() {
            last.shrink_to_fit();
        }
        self.chunks.shrink_to_fit();
    }
}

impl<T> Default for ChunkVec<T> {
    fn default() -> Self {
        ChunkVec::new()
    }
}

impl<'a, T> IntoIterator for &'a ChunkVec<T> {
    type Item = &'a T;
    type IntoIter = std::iter::Flatten<std::slice::Iter<'a, Vec<T>>>;

    fn into_iter(self) -> Self::IntoIter {
        self.chunks.iter().flatten()
    }
}

impl<T> Index<usize> for ChunkVec<T> {
    type Output = T;

    fn index(&self, index: usize) -> &T {
        &self.chunks[index / CHUNK][index % CHUNK]
    }
}

impl<T> IndexMut<usize> for ChunkVec<T> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        &mut self.chunks[index / CHUNK][index % CHUNK]
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for ChunkVec<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_across_chunk_boundaries() {
        let mut v = ChunkVec::new();
        for i in 0..(CHUNK * 2 + 5) {
            v.push(i);
        }
        assert_eq!(v.len(), CHUNK * 2 + 5);
        assert_eq!(v[CHUNK - 1], CHUNK - 1);
        assert_eq!(v[CHUNK], CHUNK);
        assert_eq!(v[CHUNK * 2 + 4], CHUNK * 2 + 4);
        assert_eq!(v.get(CHUNK * 2 + 5), None);
        v[CHUNK] = 7;
        assert_eq!(v[CHUNK], 7);
        assert_eq!(v.iter().count(), CHUNK * 2 + 5);
        *v.last_mut().unwrap() = 1;
        assert_eq!(v[CHUNK * 2 + 4], 1);
    }

    #[test]
    fn starts_empty() {
        let mut v: ChunkVec<u8> = ChunkVec::new();
        assert!(v.is_empty());
        assert!(v.last_mut().is_none());
        v.shrink_to_fit();
    }
}
