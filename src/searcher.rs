use crate::indexer::{FileEntry, FileIndexer};
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::sync::Arc;

#[derive(Clone)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub regex: bool,
    pub path_search: bool,
    pub fuzzy: bool,
    pub max_results: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            case_sensitive: false,
            regex: false,
            path_search: false,
            fuzzy: true,
            max_results: 500,
        }
    }
}

pub struct SearchResult {
    pub entry: Arc<FileEntry>,
    pub score: f32,
    pub match_type: MatchType,
}

#[derive(Clone, Copy, PartialEq)]
pub enum MatchType {
    Name,
    Path,
    Extension,
}

pub struct Searcher {
    pub options: SearchOptions,
}

#[derive(Clone, Copy, Debug)]
struct Score(f32);

impl PartialEq for Score {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for Score {}

impl Ord for Score {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl PartialOrd for Score {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct HeapItem {
    score: Score,
    tie: usize,
    result: SearchResult,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score && self.tie == other.tie
    }
}

impl Eq for HeapItem {}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .cmp(&other.score)
            .then_with(|| self.tie.cmp(&other.tie))
    }
}

#[derive(Clone, Debug)]
struct TokenMatch {
    query_index: usize,
    first: usize,
    last: usize,
    score: f32,
    needle_len: usize,
}

impl Searcher {
    pub fn new() -> Self {
        Self {
            options: SearchOptions::default(),
        }
    }

    pub fn set_options(&mut self, options: SearchOptions) {
        self.options = options;
    }

    pub fn search(&self, indexer: &FileIndexer, pattern: &str) -> Vec<SearchResult> {
        if pattern.is_empty() {
            return Vec::new();
        }

        let entries = indexer.get_entries();
        let keep = self.options.max_results.max(1);
        let mut heap: BinaryHeap<Reverse<HeapItem>> = BinaryHeap::new();

        let search_pattern = if self.options.case_sensitive {
            pattern.to_string()
        } else {
            pattern.to_lowercase()
        };
        let tokens: Vec<&str> = search_pattern.split_whitespace().filter(|t| !t.is_empty()).collect();
        if tokens.is_empty() {
            return Vec::new();
        }

        for (entry_idx, entry) in entries.iter().enumerate() {
            if self.options.path_search {
                let haystack = if self.options.case_sensitive {
                    entry.path.as_str()
                } else {
                    entry.path_lower.as_str()
                };
                if let Some(score) = self.tokens_score(haystack, &tokens) {
                    self.push_top_k(
                        &mut heap,
                        keep,
                        entry_idx,
                        entry,
                        score,
                        MatchType::Path,
                    );
                }
                continue;
            }

            let name_haystack = if self.options.case_sensitive {
                entry.name.as_str()
            } else {
                entry.name_lower.as_str()
            };
            if let Some(score) = self.tokens_score(name_haystack, &tokens) {
                self.push_top_k(
                    &mut heap,
                    keep,
                    entry_idx,
                    entry,
                    score,
                    MatchType::Name,
                );
                continue;
            }

            let path_haystack = if self.options.case_sensitive {
                entry.path.as_str()
            } else {
                entry.path_lower.as_str()
            };
            if let Some(score) = self.tokens_score(path_haystack, &tokens) {
                self.push_top_k(
                    &mut heap,
                    keep,
                    entry_idx,
                    entry,
                    score,
                    MatchType::Path,
                );
            }
        }

        let mut results: Vec<SearchResult> = heap.into_iter().map(|r| r.0.result).collect();
        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));

        results
    }

    fn push_top_k(
        &self,
        heap: &mut BinaryHeap<Reverse<HeapItem>>,
        keep: usize,
        tie: usize,
        entry: &FileEntry,
        match_score: f32,
        match_type: MatchType,
    ) {
        let final_score = self.final_score(entry, match_score, match_type);
        let item = Reverse(HeapItem {
            score: Score(final_score),
            tie,
            result: SearchResult {
                entry: Arc::new(entry.clone()),
                score: final_score,
                match_type,
            },
        });

        if heap.len() < keep {
            heap.push(item);
            return;
        }

        let Some(min_item) = heap.peek() else {
            return;
        };
        if item.0.score > min_item.0.score {
            heap.pop();
            heap.push(item);
        }
    }

    fn final_score(&self, entry: &FileEntry, match_score: f32, match_type: MatchType) -> f32 {
        let mut score = 0.0;

        // 匹配类型加权
        match match_type {
            MatchType::Name => score += 100.0,
            MatchType::Path => score += 50.0,
            MatchType::Extension => score += 30.0,
        }

        score += match_score;

        // 长度惩罚（避免长文件名排名过高）
        let len_penalty = (entry.name.len() as f32 / 100.0).min(10.0);
        score -= len_penalty;

        score
    }

    fn tokens_score(&self, haystack: &str, tokens: &[&str]) -> Option<f32> {
        if tokens.is_empty() {
            return None;
        }

        if self.options.fuzzy {
            return self.fuzzy_tokens_score(haystack, tokens);
        }

        let mut total = 0.0;
        for token in tokens {
            total += self.substring_match_score(haystack, token)?;
        }
        Some(total)
    }

    fn substring_match_score(&self, haystack: &str, token: &str) -> Option<f32> {
        if token.is_empty() {
            return None;
        }
        if haystack.starts_with(token) {
            return Some(80.0);
        }
        if haystack.contains(token) {
            return Some(50.0);
        }
        None
    }

    fn fuzzy_tokens_score(&self, haystack: &str, tokens: &[&str]) -> Option<f32> {
        let required = match tokens.len() {
            0 => return None,
            1 | 2 => tokens.len(),
            _ => tokens.len().saturating_sub(1),
        };

        let mut matches: Vec<TokenMatch> = Vec::with_capacity(tokens.len());
        let mut base = 0.0f32;
        let mut missing = 0usize;

        for (query_index, token) in tokens.iter().enumerate() {
            match self.fuzzy_token_match(haystack, token, query_index) {
                Some(m) => {
                    base += m.score;
                    matches.push(m);
                }
                None => missing += 1,
            }
        }

        if matches.len() < required {
            return None;
        }

        let mut score = base;
        score += matches.len() as f32 * 18.0;
        score -= missing as f32 * 28.0;

        if matches.len() < 2 {
            return Some(score);
        }

        let (min_first, max_last, total_needle_len) = matches.iter().fold(
            (usize::MAX, 0usize, 0usize),
            |(min_f, max_l, total_len), m| {
                (
                    min_f.min(m.first),
                    max_l.max(m.last),
                    total_len.saturating_add(m.needle_len),
                )
            },
        );
        let span = (max_last.saturating_sub(min_first) + 1).max(1) as f32;
        let compact = (total_needle_len.max(1) as f32 / span).min(1.0);

        score += compact * 90.0;
        score -= (span - total_needle_len.max(1) as f32).max(0.0) * 0.6;

        let mut by_pos: Vec<(usize, usize, usize)> = matches
            .iter()
            .map(|m| (m.query_index, m.first, m.last))
            .collect();
        by_pos.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));

        let mut inversions = 0usize;
        for i in 0..by_pos.len() {
            for j in (i + 1)..by_pos.len() {
                if by_pos[i].0 > by_pos[j].0 {
                    inversions += 1;
                }
            }
        }
        score -= inversions as f32 * 16.0;
        if inversions == 0 {
            score += 10.0;
        }

        let mut gap_sum = 0usize;
        for w in by_pos.windows(2) {
            let prev_last = w[0].2;
            let cur_first = w[1].1;
            gap_sum += cur_first.saturating_sub(prev_last + 1);
        }
        score -= gap_sum as f32 * 0.7;

        Some(score)
    }

    fn fuzzy_token_match(&self, haystack: &str, token: &str, query_index: usize) -> Option<TokenMatch> {
        if token.is_empty() {
            return None;
        }

        let m = fuzzy_match(haystack, token)?;
        let needle_len = token.chars().count().max(1);
        let span_usize = (m.last.saturating_sub(m.first) + 1).max(1);
        if needle_len <= 2 && m.gaps != 0 {
            return None;
        }
        if span_usize > needle_len.saturating_mul(10).saturating_add(20) {
            return None;
        }

        let span = span_usize as f32;
        let compact = (needle_len as f32 / span).min(1.0);
        let start_bonus = 30.0 / (1.0 + m.first as f32);
        let gap_penalty = m.gaps as f32 * 1.5;

        let mut score = 40.0 + compact * 60.0 + start_bonus - gap_penalty;
        if m.gaps == 0 {
            score += 20.0;
        }

        Some(TokenMatch {
            query_index,
            first: m.first,
            last: m.last,
            score,
            needle_len,
        })
    }
}

impl Default for Searcher {
    fn default() -> Self {
        Self::new()
    }
}

struct FuzzyMatch {
    first: usize,
    last: usize,
    gaps: usize,
}

fn fuzzy_match(haystack: &str, needle: &str) -> Option<FuzzyMatch> {
    let mut needle_iter = needle.chars();
    let mut current = needle_iter.next()?;

    let mut first: Option<usize> = None;
    let mut last: usize = 0;
    let mut prev: Option<usize> = None;
    let mut gaps: usize = 0;

    for (i, c) in haystack.chars().enumerate() {
        if c != current {
            continue;
        }

        if first.is_none() {
            first = Some(i);
        }
        if let Some(prev_i) = prev {
            gaps += i.saturating_sub(prev_i + 1);
        }
        prev = Some(i);
        last = i;

        if let Some(next) = needle_iter.next() {
            current = next;
        } else {
            return Some(FuzzyMatch {
                first: first.unwrap_or(i),
                last,
                gaps,
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, path: &str) -> FileEntry {
        FileEntry {
            name: name.to_string(),
            name_lower: name.to_lowercase(),
            path: path.to_string(),
            path_lower: path.to_lowercase(),
            size: 0,
            modified_ms: 0,
            is_dir: false,
            is_hidden: false,
        }
    }

    #[test]
    fn keyword_order_is_tolerated_in_fuzzy_mode() {
        let mut indexer = FileIndexer::new();
        indexer.set_entries_from_cache(vec![entry(
            "hello_world.txt",
            "C:/tmp/hello_world.txt",
        )]);

        let mut searcher = Searcher::new();
        searcher.options.fuzzy = true;

        let results = searcher.search(&indexer, "world hello");
        assert!(!results.is_empty());
        assert_eq!(results[0].entry.name, "hello_world.txt");
    }

    #[test]
    fn in_order_keywords_rank_higher_than_swapped() {
        let mut indexer = FileIndexer::new();
        indexer.set_entries_from_cache(vec![entry(
            "hello_world.txt",
            "C:/tmp/hello_world.txt",
        )]);

        let mut searcher = Searcher::new();
        searcher.options.fuzzy = true;

        let a = searcher.search(&indexer, "hello world");
        let b = searcher.search(&indexer, "world hello");
        assert!(!a.is_empty() && !b.is_empty());
        assert!(a[0].score > b[0].score);
    }

    #[test]
    fn allow_one_missing_keyword_when_three_or_more() {
        let mut indexer = FileIndexer::new();
        indexer.set_entries_from_cache(vec![entry(
            "hello_world.txt",
            "C:/tmp/hello_world.txt",
        )]);

        let mut searcher = Searcher::new();
        searcher.options.fuzzy = true;

        let ok = searcher.search(&indexer, "hello world extra");
        assert!(!ok.is_empty());

        let not_ok = searcher.search(&indexer, "hello world extra more");
        assert!(not_ok.is_empty());
    }

    #[test]
    fn max_results_does_not_hide_late_high_score_matches() {
        let mut entries = Vec::new();
        for i in 0..50 {
            entries.push(entry(
                &format!("h__e__l__l__o__noise_{i}.txt"),
                &format!("C:/tmp/h__e__l__l__o__noise_{i}.txt"),
            ));
        }
        entries.push(entry("hello_target.txt", "C:/tmp/hello_target.txt"));

        let mut indexer = FileIndexer::new();
        indexer.set_entries_from_cache(entries);

        let mut searcher = Searcher::new();
        searcher.options.fuzzy = true;
        searcher.options.max_results = 5;

        let results = searcher.search(&indexer, "hello");
        assert!(!results.is_empty());
        assert_eq!(results[0].entry.name, "hello_target.txt");
    }
}
