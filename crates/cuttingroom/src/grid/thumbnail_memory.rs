use std::collections::{BTreeMap, BTreeSet};

use crate::thumbnail_cache::LoadedThumbnail;

const MEMORY_LIMIT: usize = 96 * 1024 * 1024;
const MAX_ENTRIES: usize = 1024;

#[derive(Default)]
pub(super) struct ThumbnailMemory {
    scan: u64,
    bytes: usize,
    clock: u64,
    entries: BTreeMap<usize, Entry>,
    order: BTreeSet<(bool, u64, usize)>,
}

struct Entry {
    thumbnail: LoadedThumbnail,
    bytes: usize,
    used: bool,
    touched: u64,
}

impl ThumbnailMemory {
    pub(super) fn load(
        &mut self,
        scan: u64,
        index: Option<usize>,
        visible: bool,
        load: impl FnOnce() -> LoadedThumbnail,
    ) -> LoadedThumbnail {
        if self.scan != scan {
            *self = Self {
                scan,
                ..Self::default()
            };
        }
        let Some(index) = index else { return load() };
        if let Some(entry) = self.entries.get_mut(&index) {
            if visible {
                self.order.remove(&(entry.used, entry.touched, index));
                self.clock += 1;
                entry.used = true;
                entry.touched = self.clock;
                self.order.insert((true, entry.touched, index));
            }
            return entry.thumbnail.clone();
        }

        let thumbnail = load();
        if let Ok(pixels) = &thumbnail.preview.pixels {
            let bytes = std::mem::size_of_val(pixels.as_slice());
            if self.make_room(bytes, visible) {
                self.clock += 1;
                self.entries.insert(
                    index,
                    Entry {
                        thumbnail: thumbnail.clone(),
                        bytes,
                        used: visible,
                        touched: self.clock,
                    },
                );
                self.order.insert((visible, self.clock, index));
                self.bytes += bytes;
            }
        }
        thumbnail
    }

    fn make_room(&mut self, bytes: usize, visible: bool) -> bool {
        if bytes > MEMORY_LIMIT {
            return false;
        }
        while self.bytes + bytes > MEMORY_LIMIT || self.entries.len() >= MAX_ENTRIES {
            // Background warming keeps its place once the budget is full.
            if !visible {
                return false;
            }
            let Some((_, _, index)) = self.order.pop_first() else {
                return false;
            };
            if let Some(entry) = self.entries.remove(&index) {
                self.bytes -= entry.bytes;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use slint::SharedPixelBuffer;

    use super::*;
    use crate::{preview::Preview, thumbnail_cache::CacheStatus};

    #[test]
    fn warming_reuses_pixels_and_preserves_recently_viewed_thumbnails() {
        let reads = Cell::new(0);
        let load = || {
            reads.set(reads.get() + 1);
            LoadedThumbnail {
                preview: Preview {
                    pixels: Ok(SharedPixelBuffer::new(2, 2)),
                    filename: String::new(),
                    summary: Vec::new(),
                },
                status: CacheStatus::Hit,
            }
        };

        let mut memory = ThumbnailMemory::default();
        for index in 0..MAX_ENTRIES {
            memory.load(1, Some(index), false, load);
        }
        memory.load(1, Some(0), true, load);
        assert_eq!(reads.get(), MAX_ENTRIES);

        memory.load(1, Some(MAX_ENTRIES), true, load);
        memory.load(1, Some(0), true, load);
        memory.load(1, Some(2), true, load);
        assert_eq!(reads.get(), MAX_ENTRIES + 1);
        memory.load(1, Some(1), true, load);
        assert_eq!(reads.get(), MAX_ENTRIES + 2);

        memory.load(2, Some(0), true, load);
        assert_eq!(reads.get(), MAX_ENTRIES + 3);
    }
}
