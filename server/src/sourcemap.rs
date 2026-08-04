//! The position map: `.luaux` offsets ↔ generated `.luau` offsets.
//!
//! A list of **verbatim runs**. luaux's codegen copies everything outside a
//! LuauX region byte for byte and captures expressions without parsing them, so
//! every position worth forwarding lies inside a run — the things that do not
//! map are precisely the things this server answers itself.
//!
//! Generated text — `create(`, `Text = `, the `__luaux_read` wrapper — has no
//! source counterpart. Both directions return `None` there, and callers must
//! handle that rather than snapping to the nearest run. A wrong position sends
//! people to code they did not write.

/// One stretch of text that appears identically in both files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub source: usize,
    pub output: usize,
    pub length: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SourceMap {
    /// Sorted by `source`, and — because emission follows source order — by
    /// `output` as well. [`SourceMap::push`] is what keeps both true.
    runs: Vec<Run>,
    /// Constructs the builder could not place. A gap costs a feature at that
    /// position and nothing says so, which is how three of them shipped — so
    /// the count is kept and can be asserted on.
    lost: usize,
    /// Why the map stopped early, if it did. Distinct from `lost`: this is
    /// coverage abandoned wholesale rather than one construct at a time.
    abandoned: Option<&'static str>,
}

impl SourceMap {
    pub fn runs(&self) -> &[Run] {
        &self.runs
    }

    /// How many constructs could not be placed.
    pub fn lost(&self) -> usize {
        self.lost
    }

    pub fn note_lost(&mut self) {
        self.lost += 1;
    }

    /// Why coverage was abandoned wholesale, if it was.
    pub fn abandoned(&self) -> Option<&'static str> {
        self.abandoned
    }

    pub fn abandon(&mut self, reason: &'static str) {
        self.abandoned.get_or_insert(reason);
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// Records a run, if it is genuinely verbatim and does not go backwards.
    ///
    /// The text check is the safety net the whole design leans on: a run is only
    /// kept when both files really do agree over its whole length. Codegen the
    /// builder guessed wrong about therefore produces *no* run rather than a
    /// wrong one, so a mis-guess costs coverage and never correctness.
    pub fn push(&mut self, source_text: &str, output_text: &str, run: Run) -> bool {
        if run.length == 0 {
            return false;
        }

        let Some(from) = source_text.get(run.source..run.source + run.length) else {
            return false;
        };
        let Some(to) = output_text.get(run.output..run.output + run.length) else {
            return false;
        };
        if from != to {
            return false;
        }

        if let Some(last) = self.runs.last() {
            if run.source < last.source + last.length || run.output < last.output + last.length {
                return false;
            }
        }

        self.runs.push(run);
        true
    }

    /// `.luaux` offset → `.luau` offset.
    pub fn to_output(&self, offset: usize) -> Option<usize> {
        let run = self.find(offset, |run| run.source)?;
        Some(run.output + (offset - run.source))
    }

    /// `.luau` offset → `.luaux` offset.
    pub fn to_source(&self, offset: usize) -> Option<usize> {
        let run = self.find(offset, |run| run.output)?;
        Some(run.source + (offset - run.output))
    }

    /// End-exclusive variants, for the closing edge of a range.
    ///
    /// A range's end sits one past its last byte, which is outside the run when
    /// the run ends there. Treating that boundary as inside is right for an end
    /// and wrong for a start, so it is a separate call rather than a widening of
    /// the main one.
    pub fn to_output_end(&self, offset: usize) -> Option<usize> {
        let run = self.find_end(offset, |run| run.source)?;
        Some(run.output + (offset - run.source))
    }

    pub fn to_source_end(&self, offset: usize) -> Option<usize> {
        let run = self.find_end(offset, |run| run.output)?;
        Some(run.source + (offset - run.output))
    }

    /// Maps a whole range, requiring both edges to land in the *same* run.
    ///
    /// Two positions that map individually can still straddle generated text, and
    /// the range between them would then cover code the author never wrote.
    pub fn to_output_range(&self, start: usize, end: usize) -> Option<(usize, usize)> {
        // An empty range is a caret, and a caret sitting just past a run is a
        // real position in it — `{count|}` is where completion happens.
        if start == end {
            let at = self.to_output(start).or_else(|| self.to_output_end(start))?;
            return Some((at, at));
        }

        let run = self.find(start, |run| run.source)?;
        if end > run.source + run.length {
            return None;
        }
        Some((run.output + (start - run.source), run.output + (end - run.source)))
    }

    pub fn to_source_range(&self, start: usize, end: usize) -> Option<(usize, usize)> {
        if start == end {
            let at = self.to_source(start).or_else(|| self.to_source_end(start))?;
            return Some((at, at));
        }

        let run = self.find(start, |run| run.output)?;
        if end > run.output + run.length {
            return None;
        }
        Some((run.source + (start - run.output), run.source + (end - run.output)))
    }

    fn find(&self, offset: usize, key: fn(&Run) -> usize) -> Option<&Run> {
        let index = self.runs.partition_point(|run| key(run) <= offset);
        let run = self.runs.get(index.checked_sub(1)?)?;
        (offset < key(run) + run.length).then_some(run)
    }

    fn find_end(&self, offset: usize, key: fn(&Run) -> usize) -> Option<&Run> {
        let index = self.runs.partition_point(|run| key(run) < offset);
        let run = self.runs.get(index.checked_sub(1)?)?;
        (offset <= key(run) + run.length).then_some(run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A map over two strings that really do share the runs described.
    fn map(source: &str, output: &str, runs: &[(usize, usize, usize)]) -> SourceMap {
        let mut map = SourceMap::default();
        for (s, o, length) in runs.iter().copied() {
            assert!(
                map.push(source, output, Run { source: s, output: o, length }),
                "run ({s}, {o}, {length}) was rejected"
            );
        }
        map
    }

    const SOURCE: &str = "local e = <Frame Size={size}/>";
    const OUTPUT: &str = "local e = create(\"Frame\")({ Size = size })";

    fn sample() -> SourceMap {
        // The prefix, and the captured expression.
        map(
            SOURCE,
            OUTPUT,
            &[(0, 0, 10), (SOURCE.find("size}").unwrap(), OUTPUT.rfind("size").unwrap(), 4)],
        )
    }

    /// The invariant: every offset inside a run round-trips.
    #[test]
    fn every_offset_in_a_run_round_trips() {
        let map = sample();

        for run in map.runs() {
            for step in 0..run.length {
                let offset = run.source + step;
                let output = map.to_output(offset).expect("mapped");
                assert_eq!(map.to_source(output), Some(offset));
            }
        }
    }

    #[test]
    fn generated_text_maps_to_nothing() {
        let map = sample();
        // `create("Frame")({ ` exists only in the output.
        assert_eq!(map.to_source(OUTPUT.find("create").unwrap()), None);
        assert_eq!(map.to_source(OUTPUT.find("Size = ").unwrap()), None);
        // `<Frame Size={` exists only in the source.
        assert_eq!(map.to_output(SOURCE.find("<Frame").unwrap()), None);
    }

    #[test]
    fn a_run_is_refused_unless_both_sides_agree() {
        let mut map = SourceMap::default();
        // Points at `local` in the source but `creat` in the output.
        assert!(!map.push(SOURCE, OUTPUT, Run { source: 0, output: 11, length: 5 }));
        assert!(map.is_empty());
    }

    #[test]
    fn runs_may_not_go_backwards() {
        let mut map = SourceMap::default();
        assert!(map.push(SOURCE, OUTPUT, Run { source: 0, output: 0, length: 10 }));
        // Overlaps the run already recorded.
        assert!(!map.push(SOURCE, OUTPUT, Run { source: 2, output: 2, length: 3 }));
        assert_eq!(map.runs().len(), 1);
    }

    #[test]
    fn an_empty_run_is_not_a_run() {
        let mut map = SourceMap::default();
        assert!(!map.push(SOURCE, OUTPUT, Run { source: 0, output: 0, length: 0 }));
    }

    #[test]
    fn the_end_of_a_run_maps_only_as_an_end() {
        let map = sample();
        let end = 10; // one past the last byte of the `local e = ` run

        assert_eq!(map.to_output(end), None);
        assert_eq!(map.to_output_end(end), Some(10));
    }

    #[test]
    fn a_range_straddling_generated_text_is_refused() {
        let map = sample();
        let size = SOURCE.find("size}").unwrap();

        // Inside one run: fine.
        assert!(map.to_output_range(size, size + 4).is_some());
        // From the prefix into the expression: crosses `create("Frame")({ Size = `.
        assert_eq!(map.to_output_range(0, size + 4), None);
    }

    #[test]
    fn an_empty_map_maps_nothing() {
        let map = SourceMap::default();
        assert_eq!(map.to_output(0), None);
        assert_eq!(map.to_source(0), None);
        assert_eq!(map.to_output_end(0), None);
    }
}
