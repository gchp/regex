// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::borrow::{IntoCow, Cow};
use std::collections::HashMap;
use std::collections::hash_map::Iter;
use std::fmt;
use std::str::{Pattern, Searcher, SearchStep};

use unicode::str::utf8_char_width;

use compile::Program;
use parse;
use vm;
use vm::CaptureLocs;
use vm::MatchKind::{self, Exists, Location, Submatches};

use self::NamesIter::*;
use self::Regex::*;

/// Escapes all regular expression meta characters in `text`.
///
/// The string returned may be safely used as a literal in a regular
/// expression.
pub fn quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len());
    for c in text.chars() {
        if parse::is_punct(c) {
            quoted.push('\\')
        }
        quoted.push(c);
    }
    quoted
}

/// Tests if the given regular expression matches somewhere in the text given.
///
/// If there was a problem compiling the regular expression, an error is
/// returned.
///
/// To find submatches, split or replace text, you'll need to compile an
/// expression first.
pub fn is_match(regex: &str, text: &str) -> Result<bool, parse::Error> {
    Regex::new(regex).map(|r| r.is_match(text))
}

/// A compiled regular expression
///
/// It is represented as either a sequence of bytecode instructions (dynamic)
/// or as a specialized Rust function (native). It can be used to search, split
/// or replace text. All searching is done with an implicit `.*?` at the
/// beginning and end of an expression. To force an expression to match the
/// whole string (or a prefix or a suffix), you must use an anchor like `^` or
/// `$` (or `\A` and `\z`).
///
/// While this crate will handle Unicode strings (whether in the regular
/// expression or in the search text), all positions returned are **byte
/// indices**. Every byte index is guaranteed to be at a Unicode code point
/// boundary.
///
/// The lifetimes `'r` and `'t` in this crate correspond to the lifetime of a
/// compiled regular expression and text to search, respectively.
///
/// The only methods that allocate new strings are the string replacement
/// methods. All other methods (searching and splitting) return borrowed
/// pointers into the string given.
///
/// # Examples
///
/// Find the location of a US phone number:
///
/// ```rust
/// # use regex::Regex;
/// let re = Regex::new("[0-9]{3}-[0-9]{3}-[0-9]{4}").unwrap();
/// assert_eq!(re.find("phone: 111-222-3333"), Some((7, 19)));
/// ```
///
/// # Using the `std::str::StrExt` methods with `Regex`
///
/// Since `Regex` implements `Pattern`, you can use regexes with methods
/// defined on `std::str::StrExt`. For example, `is_match`, `find`, `find_iter`
/// and `split` can be replaced with `StrExt::contains`, `StrExt::find`,
/// `StrExt::match_indices` and `StrExt::split`.
///
/// Here are some examples:
///
/// ```rust
/// # use regex::Regex;
/// let re = Regex::new(r"\d+").unwrap();
/// let haystack = "a111b222c";
///
/// assert!(haystack.contains(&re));
/// assert_eq!(haystack.find(&re), Some(1));
/// assert_eq!(haystack.match_indices(&re).collect::<Vec<_>>(),
///            vec![(1, 4), (5, 8)]);
/// assert_eq!(haystack.split(&re).collect::<Vec<_>>(), vec!["a", "b", "c"]);
/// ```
#[derive(Clone)]
pub enum Regex {
    // The representation of `Regex` is exported to support the `regex!`
    // syntax extension. Do not rely on it.
    //
    // See the comments for the `program` module in `lib.rs` for a more
    // detailed explanation for what `regex!` requires.
    #[doc(hidden)]
    Dynamic(ExDynamic),
    #[doc(hidden)]
    Native(ExNative),
}

#[derive(Clone)]
#[doc(hidden)]
pub struct ExDynamic {
    original: String,
    names: Vec<Option<String>>,
    #[doc(hidden)]
    pub prog: Program
}

#[doc(hidden)]
pub struct ExNative {
    #[doc(hidden)]
    pub original: &'static str,
    #[doc(hidden)]
    pub names: &'static &'static [Option<&'static str>],
    #[doc(hidden)]
    pub prog: fn(MatchKind, &str, usize, usize) -> Vec<Option<usize>>
}

impl Copy for ExNative {}

impl Clone for ExNative {
    fn clone(&self) -> ExNative {
        *self
    }
}

impl fmt::Display for Regex {
    /// Shows the original regular expression.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl fmt::Debug for Regex {
    /// Shows the original regular expression.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Regex {
    /// Compiles a dynamic regular expression. Once compiled, it can be
    /// used repeatedly to search, split or replace text in a string.
    ///
    /// If an invalid expression is given, then an error is returned.
    pub fn new(re: &str) -> Result<Regex, parse::Error> {
        let ast = try!(parse::parse(re));
        let (prog, names) = Program::new(ast);
        Ok(Dynamic(ExDynamic {
            original: re.to_string(),
            names: names,
            prog: prog,
        }))
    }

    /// Returns true if and only if the regex matches the string given.
    ///
    /// # Example
    ///
    /// Test if some text contains at least one word with exactly 13
    /// characters:
    ///
    /// ```rust
    /// # extern crate regex; use regex::Regex;
    /// # fn main() {
    /// let text = "I categorically deny having triskaidekaphobia.";
    /// let matched = Regex::new(r"\b\w{13}\b").unwrap().is_match(text);
    /// assert!(matched);
    /// # }
    /// ```
    pub fn is_match(&self, text: &str) -> bool {
        has_match(&exec(self, Exists, text))
    }

    /// Returns the start and end byte range of the leftmost-first match in
    /// `text`. If no match exists, then `None` is returned.
    ///
    /// Note that this should only be used if you want to discover the position
    /// of the match. Testing the existence of a match is faster if you use
    /// `is_match`.
    ///
    /// # Example
    ///
    /// Find the start and end location of the first word with exactly 13
    /// characters:
    ///
    /// ```rust
    /// # extern crate regex; use regex::Regex;
    /// # fn main() {
    /// let text = "I categorically deny having triskaidekaphobia.";
    /// let pos = Regex::new(r"\b\w{13}\b").unwrap().find(text);
    /// assert_eq!(pos, Some((2, 15)));
    /// # }
    /// ```
    pub fn find(&self, text: &str) -> Option<(usize, usize)> {
        let caps = exec(self, Location, text);
        if has_match(&caps) {
            Some((caps[0].unwrap(), caps[1].unwrap()))
        } else {
            None
        }
    }

    /// Returns an iterator for each successive non-overlapping match in
    /// `text`, returning the start and end byte indices with respect to
    /// `text`.
    ///
    /// # Example
    ///
    /// Find the start and end location of every word with exactly 13
    /// characters:
    ///
    /// ```rust
    /// # extern crate regex; use regex::Regex;
    /// # fn main() {
    /// let text = "Retroactively relinquishing remunerations is reprehensible.";
    /// for pos in Regex::new(r"\b\w{13}\b").unwrap().find_iter(text) {
    ///     println!("{:?}", pos);
    /// }
    /// // Output:
    /// // (0, 13)
    /// // (14, 27)
    /// // (28, 41)
    /// // (45, 58)
    /// # }
    /// ```
    pub fn find_iter<'r, 't>(&'r self, text: &'t str) -> FindMatches<'r, 't> {
        FindMatches {
            re: self,
            search: text,
            last_end: 0,
            last_match: None,
        }
    }

    /// Returns the capture groups corresponding to the leftmost-first
    /// match in `text`. Capture group `0` always corresponds to the entire
    /// match. If no match is found, then `None` is returned.
    ///
    /// You should only use `captures` if you need access to submatches.
    /// Otherwise, `find` is faster for discovering the location of the overall
    /// match.
    ///
    /// # Examples
    ///
    /// Say you have some text with movie names and their release years,
    /// like "'Citizen Kane' (1941)". It'd be nice if we could search for text
    /// looking like that, while also extracting the movie name and its release
    /// year separately.
    ///
    /// ```rust
    /// # extern crate regex; use regex::Regex;
    /// # fn main() {
    /// let re = Regex::new(r"'([^']+)'\s+\((\d{4})\)").unwrap();
    /// let text = "Not my favorite movie: 'Citizen Kane' (1941).";
    /// let caps = re.captures(text).unwrap();
    /// assert_eq!(caps.at(1), Some("Citizen Kane"));
    /// assert_eq!(caps.at(2), Some("1941"));
    /// assert_eq!(caps.at(0), Some("'Citizen Kane' (1941)"));
    /// # }
    /// ```
    ///
    /// Note that the full match is at capture group `0`. Each subsequent
    /// capture group is indexed by the order of its opening `(`.
    ///
    /// We can make this example a bit clearer by using *named* capture groups:
    ///
    /// ```rust
    /// # extern crate regex; use regex::Regex;
    /// # fn main() {
    /// let re = Regex::new(r"'(?P<title>[^']+)'\s+\((?P<year>\d{4})\)")
    ///                .unwrap();
    /// let text = "Not my favorite movie: 'Citizen Kane' (1941).";
    /// let caps = re.captures(text).unwrap();
    /// assert_eq!(caps.name("title"), Some("Citizen Kane"));
    /// assert_eq!(caps.name("year"), Some("1941"));
    /// assert_eq!(caps.at(0), Some("'Citizen Kane' (1941)"));
    /// # }
    /// ```
    ///
    /// Here we name the capture groups, which we can access with the `name`
    /// method. Note that the named capture groups are still accessible with
    /// `at`.
    ///
    /// The `0`th capture group is always unnamed, so it must always be
    /// accessed with `at(0)`.
    pub fn captures<'t>(&self, text: &'t str) -> Option<Captures<'t>> {
        let caps = exec(self, Submatches, text);
        Captures::new(self, text, caps)
    }

    /// Returns an iterator over all the non-overlapping capture groups matched
    /// in `text`. This is operationally the same as `find_iter` (except it
    /// yields information about submatches).
    ///
    /// # Example
    ///
    /// We can use this to find all movie titles and their release years in
    /// some text, where the movie is formatted like "'Title' (xxxx)":
    ///
    /// ```rust
    /// # extern crate regex; use regex::Regex;
    /// # fn main() {
    /// let re = Regex::new(r"'(?P<title>[^']+)'\s+\((?P<year>\d{4})\)")
    ///                .unwrap();
    /// let text = "'Citizen Kane' (1941), 'The Wizard of Oz' (1939), 'M' (1931).";
    /// for caps in re.captures_iter(text) {
    ///     println!("Movie: {:?}, Released: {:?}", caps.name("title"), caps.name("year"));
    /// }
    /// // Output:
    /// // Movie: Citizen Kane, Released: 1941
    /// // Movie: The Wizard of Oz, Released: 1939
    /// // Movie: M, Released: 1931
    /// # }
    /// ```
    pub fn captures_iter<'r, 't>(&'r self, text: &'t str)
                                -> FindCaptures<'r, 't> {
        FindCaptures {
            re: self,
            search: text,
            last_match: None,
            last_end: 0,
        }
    }

    /// Returns an iterator of substrings of `text` delimited by a match
    /// of the regular expression.
    /// Namely, each element of the iterator corresponds to text that *isn't*
    /// matched by the regular expression.
    ///
    /// This method will *not* copy the text given.
    ///
    /// # Example
    ///
    /// To split a string delimited by arbitrary amounts of spaces or tabs:
    ///
    /// ```rust
    /// # extern crate regex; use regex::Regex;
    /// # fn main() {
    /// let re = Regex::new(r"[ \t]+").unwrap();
    /// let fields: Vec<&str> = re.split("a b \t  c\td    e").collect();
    /// assert_eq!(fields, vec!("a", "b", "c", "d", "e"));
    /// # }
    /// ```
    pub fn split<'r, 't>(&'r self, text: &'t str) -> RegexSplits<'r, 't> {
        RegexSplits {
            finder: self.find_iter(text),
            last: 0,
        }
    }

    /// Returns an iterator of at most `limit` substrings of `text` delimited
    /// by a match of the regular expression. (A `limit` of `0` will return no
    /// substrings.)
    /// Namely, each element of the iterator corresponds to text that *isn't*
    /// matched by the regular expression.
    /// The remainder of the string that is not split will be the last element
    /// in the iterator.
    ///
    /// This method will *not* copy the text given.
    ///
    /// # Example
    ///
    /// Get the first two words in some text:
    ///
    /// ```rust
    /// # extern crate regex; use regex::Regex;
    /// # fn main() {
    /// let re = Regex::new(r"\W+").unwrap();
    /// let fields: Vec<&str> = re.splitn("Hey! How are you?", 3).collect();
    /// assert_eq!(fields, vec!("Hey", "How", "are you?"));
    /// # }
    /// ```
    pub fn splitn<'r, 't>(&'r self, text: &'t str, limit: usize)
                         -> RegexSplitsN<'r, 't> {
        RegexSplitsN {
            splits: self.split(text),
            cur: 0,
            limit: limit,
        }
    }

    /// Replaces the leftmost-first match with the replacement provided.
    /// The replacement can be a regular string (where `$N` and `$name` are
    /// expanded to match capture groups) or a function that takes the matches'
    /// `Captures` and returns the replaced string.
    ///
    /// If no match is found, then a copy of the string is returned unchanged.
    ///
    /// # Examples
    ///
    /// Note that this function is polymorphic with respect to the replacement.
    /// In typical usage, this can just be a normal string:
    ///
    /// ```rust
    /// # extern crate regex; use regex::Regex;
    /// # fn main() {
    /// let re = Regex::new("[^01]+").unwrap();
    /// assert_eq!(re.replace("1078910", ""), "1010");
    /// # }
    /// ```
    ///
    /// But anything satisfying the `Replacer` trait will work. For example,
    /// a closure of type `|&Captures| -> String` provides direct access to the
    /// captures corresponding to a match. This allows one to access
    /// submatches easily:
    ///
    /// ```rust
    /// # extern crate regex; use regex::Regex;
    /// # use regex::Captures; fn main() {
    /// let re = Regex::new(r"([^,\s]+),\s+(\S+)").unwrap();
    /// let result = re.replace("Springsteen, Bruce", |caps: &Captures| {
    ///     format!("{} {}", caps.at(2).unwrap_or(""), caps.at(1).unwrap_or(""))
    /// });
    /// assert_eq!(result, "Bruce Springsteen");
    /// # }
    /// ```
    ///
    /// But this is a bit cumbersome to use all the time. Instead, a simple
    /// syntax is supported that expands `$name` into the corresponding capture
    /// group. Here's the last example, but using this expansion technique
    /// with named capture groups:
    ///
    /// ```rust
    /// # extern crate regex; use regex::Regex;
    /// # fn main() {
    /// let re = Regex::new(r"(?P<last>[^,\s]+),\s+(?P<first>\S+)").unwrap();
    /// let result = re.replace("Springsteen, Bruce", "$first $last");
    /// assert_eq!(result, "Bruce Springsteen");
    /// # }
    /// ```
    ///
    /// Note that using `$2` instead of `$first` or `$1` instead of `$last`
    /// would produce the same result. To write a literal `$` use `$$`.
    ///
    /// Finally, sometimes you just want to replace a literal string with no
    /// submatch expansion. This can be done by wrapping a string with
    /// `NoExpand`:
    ///
    /// ```rust
    /// # extern crate regex; use regex::Regex;
    /// # fn main() {
    /// use regex::NoExpand;
    ///
    /// let re = Regex::new(r"(?P<last>[^,\s]+),\s+(\S+)").unwrap();
    /// let result = re.replace("Springsteen, Bruce", NoExpand("$2 $last"));
    /// assert_eq!(result, "$2 $last");
    /// # }
    /// ```
    pub fn replace<R: Replacer>(&self, text: &str, rep: R) -> String {
        self.replacen(text, 1, rep)
    }

    /// Replaces all non-overlapping matches in `text` with the
    /// replacement provided. This is the same as calling `replacen` with
    /// `limit` set to `0`.
    ///
    /// See the documentation for `replace` for details on how to access
    /// submatches in the replacement string.
    pub fn replace_all<R: Replacer>(&self, text: &str, rep: R) -> String {
        self.replacen(text, 0, rep)
    }

    /// Replaces at most `limit` non-overlapping matches in `text` with the
    /// replacement provided. If `limit` is 0, then all non-overlapping matches
    /// are replaced.
    ///
    /// See the documentation for `replace` for details on how to access
    /// submatches in the replacement string.
    pub fn replacen<R: Replacer>
                   (&self, text: &str, limit: usize, mut rep: R) -> String {
        let mut new = String::with_capacity(text.len());
        let mut last_match = 0;

        for (i, cap) in self.captures_iter(text).enumerate() {
            // It'd be nicer to use the 'take' iterator instead, but it seemed
            // awkward given that '0' => no limit.
            if limit > 0 && i >= limit {
                break
            }

            let (s, e) = cap.pos(0).unwrap(); // captures only reports matches
            new.push_str(&text[last_match..s]);
            new.push_str(rep.reg_replace(&cap).as_slice());
            last_match = e;
        }
        new.push_str(&text[last_match..]);
        return new;
    }

    /// Returns the original string of this regex.
    pub fn as_str<'a>(&'a self) -> &'a str {
        match *self {
            Dynamic(ExDynamic { ref original, .. }) => original.as_slice(),
            Native(ExNative { ref original, .. }) => original.as_slice(),
        }
    }

    #[doc(hidden)]
    pub fn names_iter<'a>(&'a self) -> NamesIter<'a> {
        match *self {
            Native(ref n) => NamesIterNative(n.names.iter()),
            Dynamic(ref d) => NamesIterDynamic(d.names.iter())
        }
    }

    fn names_len(&self) -> usize {
        match *self {
            Native(ref n) => n.names.len(),
            Dynamic(ref d) => d.names.len()
        }
    }

}

pub enum NamesIter<'a> {
    NamesIterNative(::std::slice::Iter<'a, Option<&'static str>>),
    NamesIterDynamic(::std::slice::Iter<'a, Option<String>>)
}

impl<'a> Iterator for NamesIter<'a> {
    type Item=Option<String>;

    fn next(&mut self) -> Option<Option<String>> {
        match *self {
            NamesIterNative(ref mut i) => i.next().map(|x| x.map(|s| s.to_string())),
            NamesIterDynamic(ref mut i) => i.next().map(|x| x.as_ref().map(|s| s.to_string())),
        }
    }
}

/// NoExpand indicates literal string replacement.
///
/// It can be used with `replace` and `replace_all` to do a literal
/// string replacement without expanding `$name` to their corresponding
/// capture groups.
///
/// `'r` is the lifetime of the literal text.
pub struct NoExpand<'t>(pub &'t str);

/// Replacer describes types that can be used to replace matches in a string.
pub trait Replacer {
    /// Returns a possibly owned string that is used to replace the match
    /// corresponding to the `caps` capture group.
    ///
    /// The `'a` lifetime refers to the lifetime of a borrowed string when
    /// a new owned string isn't needed (e.g., for `NoExpand`).
    fn reg_replace<'a>(&'a mut self, caps: &Captures) -> Cow<'a, str>;
}

impl<'t> Replacer for NoExpand<'t> {
    fn reg_replace<'a>(&'a mut self, _: &Captures) -> Cow<'a, str> {
        let NoExpand(s) = *self;
        s.into_cow()
    }
}

impl<'t> Replacer for &'t str {
    fn reg_replace<'a>(&'a mut self, caps: &Captures) -> Cow<'a, str> {
        caps.expand(*self).into_cow()
    }
}

impl<F> Replacer for F where F: FnMut(&Captures) -> String {
    fn reg_replace<'a>(&'a mut self, caps: &Captures) -> Cow<'a, str> {
        (*self)(caps).into_cow()
    }
}

/// Yields all substrings delimited by a regular expression match.
///
/// `'r` is the lifetime of the compiled expression and `'t` is the lifetime
/// of the string being split.
pub struct RegexSplits<'r, 't> {
    finder: FindMatches<'r, 't>,
    last: usize,
}

impl<'r, 't> Iterator for RegexSplits<'r, 't> {
    type Item = &'t str;

    fn next(&mut self) -> Option<&'t str> {
        let text = self.finder.search;
        match self.finder.next() {
            None => {
                if self.last >= text.len() {
                    None
                } else {
                    let s = &text[self.last..];
                    self.last = text.len();
                    Some(s)
                }
            }
            Some((s, e)) => {
                let matched = &text[self.last..s];
                self.last = e;
                Some(matched)
            }
        }
    }
}

/// Yields at most `N` substrings delimited by a regular expression match.
///
/// The last substring will be whatever remains after splitting.
///
/// `'r` is the lifetime of the compiled expression and `'t` is the lifetime
/// of the string being split.
pub struct RegexSplitsN<'r, 't> {
    splits: RegexSplits<'r, 't>,
    cur: usize,
    limit: usize,
}

impl<'r, 't> Iterator for RegexSplitsN<'r, 't> {
    type Item = &'t str;

    fn next(&mut self) -> Option<&'t str> {
        let text = self.splits.finder.search;
        if self.cur >= self.limit {
            None
        } else {
            self.cur += 1;
            if self.cur >= self.limit {
                Some(&text[self.splits.last..])
            } else {
                self.splits.next()
            }
        }
    }
}

/// Captures represents a group of captured strings for a single match.
///
/// The 0th capture always corresponds to the entire match. Each subsequent
/// index corresponds to the next capture group in the regex.
/// If a capture group is named, then the matched string is *also* available
/// via the `name` method. (Note that the 0th capture is always unnamed and so
/// must be accessed with the `at` method.)
///
/// Positions returned from a capture group are always byte indices.
///
/// `'t` is the lifetime of the matched text.
pub struct Captures<'t> {
    text: &'t str,
    locs: CaptureLocs,
    named: Option<HashMap<String, usize>>,
}

impl<'t> Captures<'t> {
    fn new(re: &Regex, search: &'t str, locs: CaptureLocs)
          -> Option<Captures<'t>> {
        if !has_match(&locs) {
            return None
        }

        let named =
            if re.names_len() == 0 {
                None
            } else {
                let mut named = HashMap::new();
                for (i, name) in re.names_iter().enumerate() {
                    match name {
                        None => {},
                        Some(name) => {
                            named.insert(name, i);
                        }
                    }
                }
                Some(named)
            };
        Some(Captures {
            text: search,
            locs: locs,
            named: named,
        })
    }

    /// Returns the start and end positions of the Nth capture group.
    /// Returns `None` if `i` is not a valid capture group or if the capture
    /// group did not match anything.
    /// The positions returned are *always* byte indices with respect to the
    /// original string matched.
    pub fn pos(&self, i: usize) -> Option<(usize, usize)> {
        let (s, e) = (i * 2, i * 2 + 1);
        if e >= self.locs.len() || self.locs[s].is_none() {
            // VM guarantees that each pair of locations are both Some or None.
            return None
        }
        Some((self.locs[s].unwrap(), self.locs[e].unwrap()))
    }

    /// Returns the matched string for the capture group `i`.  If `i` isn't
    /// a valid capture group or didn't match anything, then `None` is
    /// returned.
    pub fn at(&self, i: usize) -> Option<&'t str> {
        match self.pos(i) {
            None => None,
            Some((s, e)) => Some(&self.text[s..e])
        }
    }

    /// Returns the matched string for the capture group named `name`.  If
    /// `name` isn't a valid capture group or didn't match anything, then
    /// `None` is returned.
    pub fn name(&self, name: &str) -> Option<&'t str> {
        match self.named {
            None => None,
            Some(ref h) => {
                match h.get(name) {
                    None => None,
                    Some(i) => self.at(*i),
                }
            }
        }
    }

    /// Creates an iterator of all the capture groups in order of appearance
    /// in the regular expression.
    pub fn iter(&'t self) -> SubCaptures<'t> {
        SubCaptures { idx: 0, caps: self, }
    }

    /// Creates an iterator of all the capture group positions in order of
    /// appearance in the regular expression. Positions are byte indices
    /// in terms of the original string matched.
    pub fn iter_pos(&'t self) -> SubCapturesPos<'t> {
        SubCapturesPos { idx: 0, caps: self, }
    }

    /// Creates an iterator of all named groups as an tuple with the group
    /// name and the value. The iterator returns these values in arbitrary
    /// order.
    pub fn iter_named(&'t self) -> SubCapturesNamed<'t> {
        SubCapturesNamed { caps: self, inner: self.named.as_ref().map(|n| n.iter()) }
    }

    /// Expands all instances of `$name` in `text` to the corresponding capture
    /// group `name`.
    ///
    /// `name` may be an integer corresponding to the index of the
    /// capture group (counted by order of opening parenthesis where `0` is the
    /// entire match) or it can be a name (consisting of letters, digits or
    /// underscores) corresponding to a named capture group.
    ///
    /// If `name` isn't a valid capture group (whether the name doesn't exist or
    /// isn't a valid index), then it is replaced with the empty string.
    ///
    /// To write a literal `$` use `$$`.
    pub fn expand(&self, text: &str) -> String {
        // How evil can you get?
        // FIXME: Don't use regexes for this. It's completely unnecessary.
        let re = Regex::new(r"(^|[^$]|\b)\$(\w+)").unwrap();
        let text = re.replace_all(text, |refs: &Captures| -> String {
            let pre = refs.at(1).unwrap_or("");
            let name = refs.at(2).unwrap_or("");
            format!("{}{}", pre,
                    match name.parse::<usize>() {
                Err(_) => self.name(name).unwrap_or("").to_string(),
                Ok(i) => self.at(i).unwrap_or("").to_string(),
            })
        });
        let re = Regex::new(r"\$\$").unwrap();
        re.replace_all(text.as_slice(), NoExpand("$"))
    }

    /// Returns the number of captured groups.
    #[inline]
    pub fn len(&self) -> usize { self.locs.len() / 2 }

    /// Returns if there are no captured groups.
    #[inline]
    pub fn is_empty(&self) -> bool { self.len() == 0 }
}

/// An iterator over capture groups for a particular match of a regular
/// expression.
///
/// `'t` is the lifetime of the matched text.
pub struct SubCaptures<'t> {
    idx: usize,
    caps: &'t Captures<'t>,
}

impl<'t> Iterator for SubCaptures<'t> {
    type Item = Option<&'t str>;

    fn next(&mut self) -> Option<Option<&'t str>> {
        if self.idx < self.caps.len() {
            self.idx += 1;
            Some(self.caps.at(self.idx - 1))
        } else {
            None
        }
    }
}

/// An iterator over capture group positions for a particular match of a
/// regular expression.
///
/// Positions are byte indices in terms of the original string matched.
///
/// `'t` is the lifetime of the matched text.
pub struct SubCapturesPos<'t> {
    idx: usize,
    caps: &'t Captures<'t>,
}

impl<'t> Iterator for SubCapturesPos<'t> {
    type Item = Option<(usize, usize)>;

    fn next(&mut self) -> Option<Option<(usize, usize)>> {
        if self.idx < self.caps.len() {
            self.idx += 1;
            Some(self.caps.pos(self.idx - 1))
        } else {
            None
        }
    }
}

/// An Iterator over named capture groups as a tuple with the group
/// name and the value.
///
/// `'t` is the lifetime of the matched text.
pub struct SubCapturesNamed<'t>{
    caps: &'t Captures<'t>,
    inner: Option<Iter<'t, String, usize>>,
}

impl<'t> Iterator for SubCapturesNamed<'t> {
    type Item = (&'t str, Option<&'t str>);

    fn next(&mut self) -> Option<(&'t str, Option<&'t str>)> {
        match self.inner.as_mut().map(|it| it.next()).unwrap_or(None) {
            Some((name, pos)) => Some((name, self.caps.at(*pos))),
            None => None
        }
    }
}

/// An iterator that yields all non-overlapping capture groups matching a
/// particular regular expression.
///
/// The iterator stops when no more matches can be found.
///
/// `'r` is the lifetime of the compiled expression and `'t` is the lifetime
/// of the matched string.
pub struct FindCaptures<'r, 't> {
    re: &'r Regex,
    search: &'t str,
    last_match: Option<usize>,
    last_end: usize,
}

impl<'r, 't> Iterator for FindCaptures<'r, 't> {
    type Item = Captures<'t>;

    fn next(&mut self) -> Option<Captures<'t>> {
        if self.last_end > self.search.len() {
            return None
        }

        let caps = exec_slice(self.re, Submatches, self.search,
                              self.last_end, self.search.len());
        let (s, e) =
            if !has_match(&caps) {
                return None
            } else {
                (caps[0].unwrap(), caps[1].unwrap())
            };

        // Don't accept empty matches immediately following a match.
        // i.e., no infinite loops please.
        if e == s && Some(self.last_end) == self.last_match {
            if self.last_end >= self.search.len() {
                return None;
            }
            self.last_end +=
                utf8_char_width(self.search.as_bytes()[self.last_end]);
            return self.next()
        }
        self.last_end = e;
        self.last_match = Some(self.last_end);
        Captures::new(self.re, self.search, caps)
    }
}

/// An iterator over all non-overlapping matches for a particular string.
///
/// The iterator yields a tuple of integers corresponding to the start and end
/// of the match. The indices are byte offsets. The iterator stops when no more
/// matches can be found.
///
/// `'r` is the lifetime of the compiled expression and `'t` is the lifetime
/// of the matched string.
pub struct FindMatches<'r, 't> {
    re: &'r Regex,
    search: &'t str,
    last_match: Option<usize>,
    last_end: usize,
}

impl<'r, 't> Iterator for FindMatches<'r, 't> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<(usize, usize)> {
        if self.last_end > self.search.len() {
            return None
        }

        let caps = exec_slice(self.re, Location, self.search,
                              self.last_end, self.search.len());
        let (s, e) =
            if !has_match(&caps) {
                return None
            } else {
                (caps[0].unwrap(), caps[1].unwrap())
            };

        // Don't accept empty matches immediately following a match.
        // i.e., no infinite loops please.
        if e == s && Some(self.last_end) == self.last_match {
            if self.last_end >= self.search.len() {
                return None;
            }
            self.last_end +=
                utf8_char_width(self.search.as_bytes()[self.last_end]);
            return self.next()
        }
        self.last_end = e;
        self.last_match = Some(self.last_end);
        Some((s, e))
    }
}

struct RegexSearcher<'r, 't> {
    it: FindMatches<'r, 't>,
    last_step_end: usize,
    next_match: Option<(usize, usize)>,
}

impl<'r, 't> Pattern<'t> for &'r Regex {
    type Searcher = RegexSearcher<'r, 't>;

    fn into_searcher(self, haystack: &'t str) -> RegexSearcher<'r, 't> {
        RegexSearcher {
            it: self.find_iter(haystack),
            last_step_end: 0,
            next_match: None,
        }
    }
}

unsafe impl<'r, 't> Searcher<'t> for RegexSearcher<'r, 't> {
    #[inline]
    fn haystack(&self) -> &'t str {
        self.it.search
    }

    #[inline]
    fn next(&mut self) -> SearchStep {
        if let Some((s, e)) = self.next_match {
            self.next_match = None;
            self.last_step_end = e;
            return SearchStep::Match(s, e);
        }
        match self.it.next() {
            None => {
                if self.last_step_end < self.haystack().len() {
                    let last = self.last_step_end;
                    self.last_step_end = self.haystack().len();
                    SearchStep::Reject(last, self.haystack().len())
                } else {
                    SearchStep::Done
                }
            }
            Some((s, e)) => {
                if s == self.last_step_end {
                    self.last_step_end = e;
                    SearchStep::Match(s, e)
                } else {
                    self.next_match = Some((s, e));
                    let last = self.last_step_end;
                    self.last_step_end = s;
                    SearchStep::Reject(last, s)
                }
            }
        }
    }
}

fn exec(re: &Regex, which: MatchKind, input: &str) -> CaptureLocs {
    exec_slice(re, which, input, 0, input.len())
}

fn exec_slice(re: &Regex, which: MatchKind,
              input: &str, s: usize, e: usize) -> CaptureLocs {
    match *re {
        Dynamic(ExDynamic { ref prog, .. }) => vm::run(which, prog, input, s, e),
        Native(ExNative { ref prog, .. }) => (*prog)(which, input, s, e),
    }
}

#[inline]
fn has_match(caps: &CaptureLocs) -> bool {
    caps.len() >= 2 && caps[0].is_some() && caps[1].is_some()
}
