use std::collections::BTreeMap;

use data_types::{CompactionLevel, ParquetFile};

/// Compares the a vec of strs with the output of a set of parquet
/// files. See docs on [`ParquetFileFormatter`] for example
/// expected output.
///
/// Designed so that failure output can be directly copy/pasted
/// into the test code as expected results.
///
/// Expects to be called about like this:
/// assert_parquet_files!(expected_lines: &[&str], &files)
#[track_caller]
pub fn assert_parquet_files<'a>(
    expected_lines: impl IntoIterator<Item = &'a str>,
    files: &[ParquetFile],
) {
    let expected_lines: Vec<String> = expected_lines.into_iter().map(|s| s.to_string()).collect();

    let actual_lines = readable_list_of_files(None, files);

    assert_eq!(
        expected_lines, actual_lines,
        "\n\nexpected:\n\n{expected_lines:#?}\nactual:\n\n{actual_lines:#?}\n\n",
    );
}

/// Compares the a vec of strs with the output of a set of parquet
/// files. This is used to compare the results of splitting files into
/// two groups. See docs on [`ParquetFileFormatter`] for example
/// expected output.
///
/// Designed so that failure output can be directly copy/pasted
/// into the test code as expected results.
///
/// Expects to be called about like this:
/// assert_parquet_files_split!(expected_lines: &[&str], &files1, &files2)
#[track_caller]
pub fn assert_parquet_files_split<'a>(
    expected_lines: impl IntoIterator<Item = &'a str>,
    files1: &[ParquetFile],
    files2: &[ParquetFile],
) {
    let expected_lines: Vec<String> = expected_lines.into_iter().map(|s| s.to_string()).collect();

    let actual_lines_one = readable_list_of_files(Some("left".into()), files1);

    let actual_lines_two = readable_list_of_files(Some("right".into()), files2);

    let actual_lines: Vec<_> = actual_lines_one
        .into_iter()
        .chain(actual_lines_two.into_iter())
        .collect();

    assert_eq!(
        expected_lines, actual_lines,
        "\n\nexpected:\n\n{expected_lines:#?}\nactual:\n\n{actual_lines:#?}\n\n",
    );
}

/// default width for printing
const DEFAULT_WIDTH: usize = 80;

/// default width for header
const DEFAULT_HEADING_WIDTH: usize = 20;

/// This function returns a visual representation of the list of
/// parquet files arranged so they are lined up horizontally based on
/// their relative time range.
///
/// See docs on [`ParquetFileFormatter`]
/// for examples.
pub fn readable_list_of_files<'a>(
    title: Option<String>,
    files: impl IntoIterator<Item = &'a ParquetFile>,
) -> Vec<String> {
    let mut output = vec![];
    if let Some(title) = title {
        output.push(title);
    }

    let files: Vec<_> = files.into_iter().collect();
    if files.is_empty() {
        return output;
    }

    let formatter = ParquetFileFormatter::new(&files);

    // split up the files into groups by levels (compaction levels)
    let mut files_by_level = BTreeMap::new();
    for file in &files {
        let existing_files = files_by_level
            .entry(file.compaction_level)
            .or_insert_with(Vec::new);
        existing_files.push(file);
    }

    for (level, files) in files_by_level {
        output.push(formatter.format_level(&level));
        for file in files {
            output.push(formatter.format_file(file))
        }
    }

    output
}

/// Formats a parquet files as a single line of text, with widths
/// normalized based on their min/max times and lined up horizontally
/// based on their relative time range.
///
/// Each file has this format:
///
/// ```text
/// L<levelno>.<id>[min_time,max_time]@file_size_bytes
/// ```
///
/// Example
///
/// ```text
/// L0
/// L0.1[100,200]@1     |----------L0.1----------|
/// L0.2[300,400]@1                                                          |----------L0.2----------|
/// L0.11[150,350]@44                |-----------------------L0.11-----------------------|
/// ```
#[derive(Debug, Default)]
struct ParquetFileFormatter {
    /// should the size of the files be shown (if they are different)
    show_size: bool,
    /// width in characater
    row_heading_chars: usize,
    /// width, in characters, of the entire min/max timerange
    width_chars: usize,
    /// how many ns are given a single character's width
    ns_per_char: f64,
    /// what is the lowest time range in any file
    min_time: i64,
    /// what is the largest time in any file?
    max_time: i64,
}

#[derive(Debug)]
/// helper to track if there are multiple file sizes in a set of parquet files
enum FileSizeSeen {
    None,
    One(i64),
    Many,
}

impl FileSizeSeen {
    fn observe(self, file_size_bytes: i64) -> Self {
        match self {
            Self::None => Self::One(file_size_bytes),
            // same file size?
            Self::One(sz) if sz == file_size_bytes => Self::One(sz),
            // different file size or already seen difference
            Self::One(_) | Self::Many => Self::Many,
        }
    }
}

impl ParquetFileFormatter {
    /// calculates display parameters for formatting a set of files
    fn new(files: &[&ParquetFile]) -> Self {
        let row_heading_chars = DEFAULT_HEADING_WIDTH;
        let width_chars = DEFAULT_WIDTH;

        let min_time = files
            .iter()
            .map(|f| f.min_time.get())
            .min()
            .expect("at least one file");
        let max_time = files
            .iter()
            .map(|f| f.max_time.get())
            .max()
            .expect("at least one file");
        let file_size_seen = files
            .iter()
            .fold(FileSizeSeen::None, |file_size_seen, file| {
                file_size_seen.observe(file.file_size_bytes)
            });

        // show the size if there are multiple sizes
        let show_size = matches!(file_size_seen, FileSizeSeen::Many);

        let time_range = max_time - min_time;

        let ns_per_char = (time_range as f64) / (width_chars as f64);

        Self {
            show_size,
            width_chars,
            ns_per_char,
            min_time,
            max_time,
            row_heading_chars,
        }
    }

    /// return how many characters of `self.width_chars` would be consumed by `range` ns
    fn time_range_to_chars(&self, time_range: i64) -> usize {
        // avoid divide by zero
        if self.ns_per_char > 0.0 {
            (time_range as f64 / self.ns_per_char) as usize
        } else if time_range > 0 {
            self.width_chars
        } else {
            0
        }
    }

    fn format_level(&self, level: &CompactionLevel) -> String {
        format!(
            "{:width$}",
            display_level(level),
            width = self.width_chars + self.row_heading_chars
        )
    }

    /// Formats a single parquet file into a string of `width_chars`
    /// characters, which tries to visually depict the timge range of
    /// the file using the width. See docs on [`ParquetFileFormatter`]
    /// for examples.
    fn format_file(&self, file: &ParquetFile) -> String {
        // use try_into to force conversion to usize
        let time_width = (file.max_time - file.min_time).get();

        // special case "zero" width times
        let field_width = if self.min_time == self.max_time {
            self.width_chars
        } else {
            self.time_range_to_chars(time_width)
        }
        // account for starting and ending '|'
        .saturating_sub(2);

        // Get compact display of the file, like 'L0.1'
        // add |--- ---| formatting (based on field width)
        let file_string = format!("|{:-^width$}|", display_file_id(file), width = field_width);
        let row_heading = display_format(file, self.show_size);

        // special case "zero" width times
        if self.min_time == self.max_time {
            return format!(
                "{row_heading:width1$}{file_string:^width2$}",
                width1 = self.row_heading_chars,
                width2 = self.width_chars,
            );
        }

        // otherwise, figure out whitespace padding at start and back
        // based on the relative start time of the file
        // assume time from 0
        let prefix_time_range = file.min_time.get().saturating_sub(self.min_time);
        let prefix_padding = " ".repeat(self.time_range_to_chars(prefix_time_range));

        // pad the rest with whitespace
        let postfix_padding_len = self
            .width_chars
            .saturating_sub(file_string.len())
            .saturating_sub(prefix_padding.len());
        let postfix_padding = " ".repeat(postfix_padding_len);

        format!(
            "{row_heading:width$}{prefix_padding}{file_string}{postfix_padding}",
            width = self.row_heading_chars
        )
    }
}

fn display_level(compaction_level: &CompactionLevel) -> &'static str {
    match compaction_level {
        CompactionLevel::Initial => "L0",
        CompactionLevel::FileNonOverlapped => "L1",
        CompactionLevel::Final => "L2",
    }
}

/// Display like 'L0.1' with file level and id
fn display_file_id(file: &ParquetFile) -> String {
    let level = display_level(&file.compaction_level);
    let id = file.id;
    format!("{level}.{id}")
}

/// Compact display of level, id min/max time and optional size.
///
/// Example
///
/// ```text
/// L0.1[100,200]@1
/// ```
fn display_format(file: &ParquetFile, show_size: bool) -> String {
    let file_id = display_file_id(file);
    let min_time = file.min_time.get(); // display as i64
    let max_time = file.max_time.get(); // display as i64
    let sz = file.file_size_bytes;
    if show_size {
        format!("{file_id}[{min_time},{max_time}]@{sz}")
    } else {
        format!("{file_id}[{min_time},{max_time}]")
    }
}

#[cfg(test)]
mod test {
    use crate::test_util::ParquetFileBuilder;

    use super::*;

    #[test]
    fn display_builder() {
        let files = vec![
            ParquetFileBuilder::new(1)
                .with_compaction_level(CompactionLevel::Initial)
                .build(),
            ParquetFileBuilder::new(2)
                .with_compaction_level(CompactionLevel::Initial)
                .build(),
        ];

        let expected = vec![
            "L0                                                                                                  ",
            "L0.1[0,0]           |-------------------------------------L0.1-------------------------------------|",
            "L0.2[0,0]           |-------------------------------------L0.2-------------------------------------|",
        ];

        assert_parquet_files(expected, &files);
    }

    #[test]
    fn display_builder_multi_levels_with_size() {
        let files = vec![
            ParquetFileBuilder::new(1)
                .with_compaction_level(CompactionLevel::Initial)
                .build(),
            ParquetFileBuilder::new(2)
                .with_compaction_level(CompactionLevel::Initial)
                .build(),
            ParquetFileBuilder::new(3)
                .with_compaction_level(CompactionLevel::Final)
                .with_file_size_bytes(42)
                .build(),
        ];

        let expected = vec![
            "L0                                                                                                  ",
            "L0.1[0,0]@1         |-------------------------------------L0.1-------------------------------------|",
            "L0.2[0,0]@1         |-------------------------------------L0.2-------------------------------------|",
            "L2                                                                                                  ",
            "L2.3[0,0]@42        |-------------------------------------L2.3-------------------------------------|",
        ];

        assert_parquet_files(expected, &files);
    }

    #[test]
    fn display_builder_size_time_ranges() {
        let files = vec![
            ParquetFileBuilder::new(1)
                .with_compaction_level(CompactionLevel::Initial)
                .with_time_range(100, 200)
                .build(),
            ParquetFileBuilder::new(2)
                .with_compaction_level(CompactionLevel::Initial)
                .with_time_range(300, 400)
                .build(),
            // overlapping file
            ParquetFileBuilder::new(11)
                .with_compaction_level(CompactionLevel::Initial)
                .with_time_range(150, 350)
                .with_file_size_bytes(44)
                .build(),
        ];

        let expected = vec![
            "L0                                                                                                  ",
            "L0.1[100,200]@1     |----------L0.1----------|                                                      ",
            "L0.2[300,400]@1                                                          |----------L0.2----------| ",
            "L0.11[150,350]@44                |-----------------------L0.11-----------------------|              ",
        ];

        assert_parquet_files(expected, &files);
    }
}