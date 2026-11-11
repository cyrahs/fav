//! Traditional to Simplified Chinese, exactly as `opencc-python-reimplemented` 0.1.7's
//! `OpenCC('t2s')` converts.
//!
//! Python origin: the package's `opencc.py` with its `t2s.json` configuration, used by
//! `src/tool/runtime_config.py` (`to_simplified_chinese`) and `src/web/hanime1.py`.
//! Hanime1's directory and file names depend on the output, so the dictionaries are the
//! package's own files (`opencc/TSPhrases.txt`, `opencc/TSCharacters.txt`, Apache 2.0, see
//! `opencc/NOTICE.txt`) and the matching is the package's, quirks included:
//!
//! - the text is split at the package's sentence separators (Python `\s+` and a fixed set of
//!   punctuation); separators pass through and each run between them is converted alone;
//! - the two dictionaries form one group: a run is matched against the phrases first, then
//!   whatever no phrase matched against the characters, and a converted piece is never
//!   matched again;
//! - a match is the longest key found anywhere in the piece, the leftmost of that length;
//!   the pieces left and right of it are then matched with keys no longer than the match
//!   (the package's `length_hint`), while unmatched pieces go on to the next dictionary
//!   with no limit;
//! - a value with several space-separated candidates gives the first.
//!
//! `goldens/hanime1/t2s.json` holds the package's output for every dictionary key and
//! a few thousand mixed strings; `t2s_dictionaries.json` pins the files.

use std::collections::HashMap;
use std::sync::LazyLock;

use fav_core::pytext::{is_py_space, py_strip};

const PHRASES: &str = include_str!("opencc/TSPhrases.txt");
const CHARACTERS: &str = include_str!("opencc/TSCharacters.txt");

/// The separators of `OpenCC.split_chars_re` other than `\s`.
const SEPARATORS: &str =
    "-,.?!*　，。、；：？！…“”‘’『』「」﹁﹂—－（）《》〈〉～．／＼︒︑︔︓︿﹀︹︺︙︐［﹇］﹈︕︖︰︳︴︽︾︵︶｛︷｝︸﹃﹄【︻】︼";

/// One dictionary as `_add_dictionaries` loads it: `(max_len, min_len, map_dict)`.
#[derive(Debug)]
pub struct Dictionary {
    pub max_len: usize,
    pub min_len: usize,
    pub entries: HashMap<String, String>,
}

impl Dictionary {
    /// `key, value = line.strip().split('\t')` for every line; a later key replaces an
    /// earlier one. Lengths count code points.
    fn parse(text: &str) -> Self {
        let mut entries = HashMap::new();
        let mut max_len = 1;
        let mut min_len = 1000;
        for line in text.split_inclusive('\n') {
            let mut fields = py_strip(line).split('\t');
            let (Some(key), Some(value), None) = (fields.next(), fields.next(), fields.next()) else {
                panic!("malformed dictionary line: {line:?}");
            };
            let len = key.chars().count();
            max_len = max_len.max(len);
            min_len = min_len.min(len);
            entries.insert(key.to_owned(), value.to_owned());
        }
        Self { max_len, min_len, entries }
    }
}

/// The `t2s` group: phrases, then characters.
pub static T2S: LazyLock<[Dictionary; 2]> = LazyLock::new(|| [Dictionary::parse(PHRASES), Dictionary::parse(CHARACTERS)]);

/// The embedded dictionary files, for the test that pins them to the package's.
pub fn dictionary_files() -> [(&'static str, &'static str); 2] {
    [("TSPhrases.txt", PHRASES), ("TSCharacters.txt", CHARACTERS)]
}

fn is_separator(ch: char) -> bool {
    is_py_space(ch) || SEPARATORS.contains(ch)
}

/// `OpenCC('t2s').convert(text)`.
pub fn convert(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run_start = 0;
    for (index, ch) in text.char_indices() {
        if is_separator(ch) {
            convert_run(&text[run_start..index], &mut out);
            out.push(ch);
            run_start = index + ch.len_utf8();
        }
    }
    convert_run(&text[run_start..], &mut out);
    out
}

/// `to_simplified_chinese(text)`: `None` for an empty input or an empty (stripped) result.
pub fn to_simplified_chinese(text: Option<&str>) -> Option<String> {
    let text = text.filter(|text| !text.is_empty())?;
    let converted = convert(text);
    let stripped = py_strip(&converted);
    (!stripped.is_empty()).then(|| stripped.to_owned())
}

/// What is left to do, in output order (an explicit stack instead of the package's tree).
enum Task {
    /// Convert `chars[start..end]` with dictionary `dict` and up, keys at most `hint` long.
    Convert {
        start: usize,
        end: usize,
        dict: usize,
        hint: Option<usize>,
    },
    Emit(&'static str),
}

/// `_convert(run, dict_chain_data)` for one separator-free run.
fn convert_run(run: &str, out: &mut String) {
    if run.is_empty() {
        return;
    }
    let dictionaries = &*T2S;
    // Byte offset of every code point, plus the end.
    let bounds: Vec<usize> = run.char_indices().map(|(index, _)| index).chain([run.len()]).collect();
    let mut stack = vec![Task::Convert {
        start: 0,
        end: bounds.len() - 1,
        dict: 0,
        hint: None,
    }];
    while let Some(task) = stack.pop() {
        match task {
            Task::Emit(value) => out.push_str(value),
            Task::Convert { start, end, dict, hint } => {
                let Some(dictionary) = dictionaries.get(dict) else {
                    out.push_str(&run[bounds[start]..bounds[end]]);
                    continue;
                };
                match find_match(run, &bounds, start, end, dictionary, hint) {
                    Some((at, len, value)) => {
                        // Left first: pushed last.
                        if at + len < end {
                            stack.push(Task::Convert {
                                start: at + len,
                                end,
                                dict,
                                hint: Some(len),
                            });
                        }
                        stack.push(Task::Emit(value));
                        if at > start {
                            stack.push(Task::Convert {
                                start,
                                end: at,
                                dict,
                                hint: Some(len),
                            });
                        }
                    }
                    None => stack.push(Task::Convert {
                        start,
                        end,
                        dict: dict + 1,
                        hint: None,
                    }),
                }
            }
        }
    }
}

/// `StringTree.__findMatch`: the longest key (at most `hint` long), leftmost first; its
/// position, length and first value. An empty value counts as no match (`if (value)`).
fn find_match(
    run: &str,
    bounds: &[usize],
    start: usize,
    end: usize,
    dictionary: &'static Dictionary,
    hint: Option<usize>,
) -> Option<(usize, usize, &'static str)> {
    let len = end - start;
    let mut test_len = len.min(dictionary.max_len);
    if let Some(hint) = hint {
        test_len = test_len.min(hint);
    }
    while test_len >= dictionary.min_len {
        for at in start..=end - test_len {
            if let Some(value) = dictionary.entries.get(&run[bounds[at]..bounds[at + test_len]]) {
                let first = value.split(' ').next().unwrap_or_default();
                return (!first.is_empty()).then_some((at, test_len, first));
            }
        }
        test_len -= 1;
    }
    None
}

#[cfg(test)]
mod tests;
