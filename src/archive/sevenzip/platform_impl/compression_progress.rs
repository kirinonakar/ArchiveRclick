//! Indexed source-byte ranges for progress callbacks from codec workers.

pub(super) struct CompressionProgressIndex {
    // Only nonempty entries have a visible byte range. Keep original item
    // indices so directories/empty files do not shift the displayed name.
    ranges: Vec<(u64, usize)>,
    file_ends: Vec<u64>,
}

impl CompressionProgressIndex {
    pub(super) fn new(items: impl IntoIterator<Item = (u64, bool)>) -> Self {
        let mut ranges = Vec::new();
        let mut file_ends = Vec::new();
        let mut total = 0u64;
        for (index, (size, is_file)) in items.into_iter().enumerate() {
            // Directories do not contribute source bytes.
            if !is_file {
                continue;
            }
            total = total.saturating_add(size);
            file_ends.push(total);
            if size != 0 {
                ranges.push((total, index));
            }
        }
        Self { ranges, file_ends }
    }

    pub(super) fn current_file(&self, completed: u64) -> Option<(usize, u64)> {
        if self.ranges.is_empty() {
            return None;
        }
        // At an exact boundary keep the completed file visible until work on
        // the next file starts. This also handles counters arriving out of order.
        let position = self
            .ranges
            .partition_point(|&(end, _)| end < completed)
            .min(self.ranges.len() - 1);
        let (end, index) = self.ranges[position];
        let start = position
            .checked_sub(1)
            .map_or(0, |previous| self.ranges[previous].0);
        Some((index, completed.min(end).saturating_sub(start)))
    }

    pub(super) fn completed_files(&self, completed: u64) -> u64 {
        // As before, empty files count only after source-byte work has begun;
        // final completion accounts for archives containing only empty files.
        if completed == 0 {
            return 0;
        }
        self.file_ends.partition_point(|&end| end <= completed) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::CompressionProgressIndex;

    #[test]
    fn indexed_progress_matches_source_ranges_including_empty_files_and_directories() {
        // Exhaustively check short manifests, including backward/repeated
        // counters. Queries must not depend on a mutable forward-only cursor.
        for manifest in 0..4usize.pow(6) {
            let mut code = manifest;
            let items: Vec<_> = (0..6)
                .map(|_| {
                    let item = match code % 4 {
                        0 => (0, false),
                        1 => (0, true),
                        2 => (1, true),
                        _ => (3, true),
                    };
                    code /= 4;
                    item
                })
                .collect();
            let index = CompressionProgressIndex::new(items.iter().copied());
            for completed in (0..=20).rev() {
                let mut total = 0;
                let mut expected_count = 0;
                let mut expected_file = None;
                let mut found = false;
                for (item_index, &(size, is_file)) in items.iter().enumerate() {
                    let start = total;
                    total += size;
                    if is_file && completed > 0 && total <= completed {
                        expected_count += 1;
                    }
                    if size > 0 && !found {
                        expected_file =
                            Some((item_index, completed.saturating_sub(start).min(size)));
                        found = completed <= total;
                    }
                }
                assert_eq!(index.current_file(completed), expected_file);
                assert_eq!(index.completed_files(completed), expected_count);
            }
        }
        let empty = CompressionProgressIndex::new([]);
        assert_eq!(empty.current_file(1), None);
        assert_eq!(empty.completed_files(1), 0);
    }
}
