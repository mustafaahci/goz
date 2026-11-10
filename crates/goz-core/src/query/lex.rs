//! The tokens of a query, read as Everything 1.5 reads them: search text,
//! the operators `|` (or `or:`), `!` (or `not:`) and `and:`, groups `< >`,
//! and the prefixes of functions and modifiers (`ext:`, `path:`, ...).
//!
//! White space separates terms, except inside quotes; a quote toggles
//! quoting anywhere in a term, and quoted text is literal. Outside quotes,
//! `|` ends a term and is an operator wherever it stands; `!` is the NOT
//! operator only where a term starts (`wow!.txt` is a name); `<` opens a
//! group only where a term starts, and inside a word it is an error, since
//! Everything's group expansion (`gr<a|e>y`) is not supported; `>` closes
//! the innermost group wherever it stands. A character entity (`&vert:`,
//! `&#x21:`, ...) stands for one literal character, in quotes too, as
//! Everything replaces them there as well.
//!
//! Three directives are read here and leave no token: `case:` and
//! `matchpath:`, whole-query switches as goz has always had them, and
//! `?and-precedence:` / `?or-precedence:`, which set how tightly AND binds
//! for the operators after them. `no-!:` makes the `!` of the term it
//! prefixes literal, and `?no-!:` of every term after it.

use super::parse::QueryError;
use crate::wtf8::encode_code_point;

/// One token, with where it stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Token {
    pub(super) kind: Tok,
    /// The 1-based character column it starts at, for errors.
    pub(super) column: usize,
    /// Whether white space separates it from the token before it.
    pub(super) spaced: bool,
    /// Whether AND binds tighter than OR where it stands: a
    /// `?and-precedence:` came before it, and no `?or-precedence:` since.
    pub(super) and_first: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Tok {
    /// `|` or `or:`.
    Or,
    /// `and:`; white space between terms is an AND too, with no token.
    And,
    /// `!` where a term starts, or `not:`.
    Not,
    /// `<` where a term starts.
    Open,
    /// `>` outside quotes.
    Close,
    /// A function or modifier prefix, its colon included. A function's value
    /// follows as `Text`, after any `!`s of its own (`ext:!pdf`).
    Prefix(Prefix),
    /// Search text, or a function's value.
    Text(Text),
}

/// The prefixes goz knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Prefix {
    /// `path:`: the text it prefixes matches the full path.
    Path,
    /// `file:`: files only.
    File,
    /// `folder:`: folders only.
    Folder,
    /// `dir:`: folders only, as `file:` is files only (see the parser for
    /// how the two differ from `folder:` under a `!`).
    Dir,
    /// `ext:`, a function with a value.
    Ext,
    /// `size:`, a function with a value.
    Size,
    /// `dm:`, `datemodified:` or `date-modified:`, a function with a value.
    Date,
}

impl Prefix {
    /// The prefix a name stands for, lower-cased with its hyphens dropped,
    /// as Everything's are optional.
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "path" => Prefix::Path,
            "file" => Prefix::File,
            "folder" => Prefix::Folder,
            "dir" => Prefix::Dir,
            "ext" => Prefix::Ext,
            "size" => Prefix::Size,
            "dm" | "datemodified" => Prefix::Date,
            _ => return None,
        })
    }

    /// Whether a value follows: a function rather than a modifier.
    fn takes_value(self) -> bool {
        matches!(self, Prefix::Ext | Prefix::Size | Prefix::Date)
    }

    /// Whether the value is a number or a date, which may start with a
    /// comparison: `size:>1mb`, `dm:<2020`.
    fn compares(self) -> bool {
        matches!(self, Prefix::Size | Prefix::Date)
    }

    /// The prefix as an error names it.
    pub(super) fn spelling(self) -> &'static str {
        match self {
            Prefix::Path => "path:",
            Prefix::File => "file:",
            Prefix::Folder => "folder:",
            Prefix::Dir => "dir:",
            Prefix::Ext => "ext:",
            Prefix::Size => "size:",
            Prefix::Date => "dm:",
        }
    }
}

/// Search text or a function's value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Text {
    /// WTF-8, quotes removed and entities replaced: an entity can name a
    /// lone surrogate, which a name can hold.
    pub(super) text: Vec<u8>,
    /// `&ast:` or `&quest:` stood in it, which makes every `*` and `?` of
    /// the term literal, as in Everything.
    pub(super) literal_wild: bool,
}

/// A lexed query: its tokens, and the whole-query directives it holds, the
/// last of each winning.
#[derive(Debug, Default)]
pub(super) struct Lexed {
    pub(super) tokens: Vec<Token>,
    pub(super) match_case: Option<bool>,
    pub(super) match_path: Option<bool>,
}

/// Reads `input` into tokens.
pub(super) fn lex(input: &str) -> Result<Lexed, QueryError> {
    let mut lexer = Lexer {
        chars: input.chars().collect(),
        at: 0,
        out: Lexed::default(),
        spaced: false,
        and_first: false,
        literal_bang: false,
        after_close: false,
    };
    lexer.run()?;
    Ok(lexer.out)
}

struct Lexer {
    chars: Vec<char>,
    at: usize,
    out: Lexed,
    /// White space came since the last token.
    spaced: bool,
    /// AND binds tighter than OR from here on.
    and_first: bool,
    /// `?no-!:` came: `!` is literal from here on.
    literal_bang: bool,
    /// The last token was a `>` and no white space has come since: text
    /// here would join the group, which is group expansion.
    after_close: bool,
}

/// What a `NAME:` at a term start is.
enum Head {
    Keyword(Tok),
    Prefix(Prefix),
    /// `case:` or `matchpath:`, whose value follows.
    Directive(Directive),
    /// `?and-precedence:` (true) or `?or-precedence:` (false).
    Precedence(bool),
    /// `no-!:`, or with `global`, `?no-!:`.
    NoBang {
        global: bool,
    },
}

#[derive(Clone, Copy)]
enum Directive {
    Case,
    MatchPath,
}

impl Lexer {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn emit(&mut self, kind: Tok, column: usize) {
        self.after_close = kind == Tok::Close;
        self.out.tokens.push(Token {
            kind,
            column,
            spaced: std::mem::take(&mut self.spaced),
            and_first: self.and_first,
        });
    }

    fn run(&mut self) -> Result<(), QueryError> {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.at += 1;
                self.spaced = true;
                self.after_close = false;
            } else {
                self.word()?;
            }
        }
        Ok(())
    }

    /// Reads one white-space-free word: its operators, prefixes, groups and
    /// text.
    fn word(&mut self) -> Result<(), QueryError> {
        // A term starts here: after white space, `|`, `<`, `!` or a prefix.
        // `first`: no prefix yet in this term, so a name here may be a
        // keyword, a directive, or a function goz does not know.
        let mut first = true;
        let mut literal_bang = self.literal_bang;
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                return Ok(());
            }
            let column = self.at + 1;
            if self.after_close && !matches!(c, '|' | '>') {
                return Err(QueryError::GroupExpansion { column });
            }
            match c {
                '|' => {
                    self.at += 1;
                    self.emit(Tok::Or, column);
                    first = true;
                    literal_bang = self.literal_bang;
                    continue;
                }
                '>' => {
                    self.at += 1;
                    self.emit(Tok::Close, column);
                    continue;
                }
                '<' => {
                    self.at += 1;
                    self.emit(Tok::Open, column);
                    first = true;
                    continue;
                }
                '!' if !literal_bang => {
                    self.at += 1;
                    self.emit(Tok::Not, column);
                    continue;
                }
                _ => {}
            }
            if let Some((head, after)) = self.head(first)? {
                match head {
                    Head::Keyword(Tok::Not) => {
                        self.at = after;
                        self.emit(Tok::Not, column);
                    }
                    Head::Keyword(kind) => {
                        self.at = after;
                        self.emit(kind, column);
                        first = true;
                        literal_bang = self.literal_bang;
                    }
                    Head::Precedence(and_first) => {
                        self.at = after;
                        self.and_first = and_first;
                    }
                    Head::NoBang { global } => {
                        self.at = after;
                        self.literal_bang |= global;
                        literal_bang = true;
                    }
                    Head::Directive(directive) => {
                        self.at = after;
                        self.directive(directive)?;
                    }
                    Head::Prefix(prefix) => {
                        self.at = after;
                        self.emit(Tok::Prefix(prefix), column);
                        first = false;
                        if prefix.takes_value() {
                            self.value(prefix, literal_bang)?;
                        }
                    }
                }
                continue;
            }
            let text = self.text(false)?;
            self.emit(Tok::Text(text), column);
        }
        Ok(())
    }

    /// The `NAME:` at the cursor, when it is one goz reads here, and where
    /// what follows its colon starts. `first`: no prefix came before it in
    /// the term. There, a name of two or more letters that names nothing is
    /// refused, as it always was: searched as text it could match nothing,
    /// since no name holds `:`. After a prefix, such text is the prefix's
    /// value (`file:a:b`), as it always was.
    fn head(&self, first: bool) -> Result<Option<(Head, usize)>, QueryError> {
        let start = self.at;
        let mut end = start;
        if self.chars.get(end) == Some(&'?') {
            end += 1;
        }
        while self
            .chars
            .get(end)
            .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '!'))
        {
            end += 1;
        }
        if self.chars.get(end) != Some(&':') {
            return Ok(None);
        }
        let raw: String = self.chars[start..end].iter().collect();
        let global = raw.starts_with('?');
        let name: String = raw
            .trim_start_matches('?')
            .chars()
            .filter(|&c| c != '-')
            .map(|c| c.to_ascii_lowercase())
            .collect();
        let after = end + 1;
        let head = match (global, name.as_str()) {
            (_, "andprecedence") if first => Head::Precedence(true),
            (_, "orprecedence") if first => Head::Precedence(false),
            (global, "no!") => Head::NoBang { global },
            (false, "and") if first => Head::Keyword(Tok::And),
            (false, "or") if first => Head::Keyword(Tok::Or),
            (false, "not") => Head::Keyword(Tok::Not),
            (false, "case") if first => Head::Directive(Directive::Case),
            (false, "matchpath") if first => Head::Directive(Directive::MatchPath),
            (false, name) => match Prefix::named(name) {
                Some(prefix) => Head::Prefix(prefix),
                None if first && raw.len() > 1 && raw.bytes().all(|b| b.is_ascii_alphabetic()) => {
                    return Err(QueryError::UnknownFunction(format!("{raw}:")));
                }
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        Ok(Some((head, after)))
    }

    /// Reads a `case:` or `matchpath:` directive's value, which must be
    /// empty, `0` or `1`, and records it. A directive is a switch for the
    /// whole query, so it leaves no token, wherever it stands.
    fn directive(&mut self, directive: Directive) -> Result<(), QueryError> {
        let value = self.text(false)?.text;
        let on = match value.as_slice() {
            b"" | b"1" => true,
            b"0" => false,
            _ => {
                let (what, name) = match directive {
                    Directive::Case => ("case:", "case"),
                    Directive::MatchPath => ("matchpath:", "matchpath"),
                };
                return Err(QueryError::Malformed {
                    what,
                    detail: format!(
                        "expected {name}:, {name}:0 or {name}:1, got {name}:{}",
                        String::from_utf8_lossy(&value)
                    ),
                });
            }
        };
        match directive {
            Directive::Case => self.out.match_case = Some(on),
            Directive::MatchPath => self.out.match_path = Some(on),
        }
        Ok(())
    }

    /// Reads a function's value: its own `!`s first, as NOT tokens
    /// (`ext:!pdf` is `!ext:pdf`), then the value as text. A number or a
    /// date may start with its comparison (`size:>1mb`), whose `<` or `>` is
    /// part of the value, not a group.
    fn value(&mut self, prefix: Prefix, literal_bang: bool) -> Result<(), QueryError> {
        while !literal_bang && self.peek() == Some('!') {
            let column = self.at + 1;
            self.at += 1;
            self.emit(Tok::Not, column);
        }
        let column = self.at + 1;
        if !prefix.compares() && self.peek() == Some('<') {
            return Err(QueryError::Malformed {
                what: prefix.spelling(),
                detail: format!(
                    "a value group such as {0}<a|b> at column {column} is not supported; \
                     write <{0}a|{0}b>",
                    prefix.spelling()
                ),
            });
        }
        let text = self.text(prefix.compares())?;
        self.emit(Tok::Text(text), column);
        Ok(())
    }

    /// Reads text up to unquoted white space, `|` or `>`. `comparison`: a
    /// leading run of `<`, `>` and `=` is text too.
    fn text(&mut self, comparison: bool) -> Result<Text, QueryError> {
        let mut out = Text::default();
        if comparison {
            while let Some(c @ ('<' | '>' | '=')) = self.peek() {
                out.text.push(c as u8);
                self.at += 1;
            }
        }
        let mut quoted = false;
        while let Some(c) = self.peek() {
            if c == '"' {
                quoted = !quoted;
                self.at += 1;
                continue;
            }
            if !quoted {
                if c.is_whitespace() || c == '|' || c == '>' {
                    break;
                }
                if c == '<' {
                    return Err(QueryError::GroupExpansion {
                        column: self.at + 1,
                    });
                }
            }
            if c == '&'
                && let Some((cp, len, wild)) = self.entity()
            {
                encode_code_point(cp, &mut out.text);
                out.literal_wild |= wild;
                self.at += len;
                continue;
            }
            let mut buf = [0u8; 4];
            out.text
                .extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            self.at += 1;
        }
        Ok(out)
    }

    /// The character entity at the cursor (an `&`): its code point, its
    /// length in characters, and whether it is `&ast:` or `&quest:`. `None`
    /// when the text is no entity, which leaves the `&` literal.
    fn entity(&self) -> Option<(u32, usize, bool)> {
        // The longest entity name is `#x10FFFF` with leading zeros allowed;
        // a dozen characters cover every real one.
        let rest = &self.chars[self.at + 1..];
        let colon = rest.iter().take(16).position(|&c| c == ':')?;
        let name: String = rest[..colon]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();
        let len = colon + 2;
        let named = |cp: char| Some((cp as u32, len, false));
        match name.as_str() {
            "sp" => named(' '),
            "vert" => named('|'),
            "excl" => named('!'),
            "lt" => named('<'),
            "gt" => named('>'),
            "quot" => named('"'),
            "amp" => named('&'),
            "ast" => Some(('*' as u32, len, true)),
            "quest" => Some(('?' as u32, len, true)),
            _ => {
                let number = name.strip_prefix('#')?;
                let cp = match number.strip_prefix('x') {
                    Some(hex) if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
                        u32::from_str_radix(hex, 16).ok()?
                    }
                    Some(_) => return None,
                    None if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) => {
                        number.parse::<u32>().ok()?
                    }
                    None => return None,
                };
                (cp <= 0x10FFFF).then_some((cp, len, false))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tokens of `input`, written compactly: `|`, `&`, `!`, `<`, `>`,
    /// `P(name)`, `T(text)`, each after a space when spaced.
    fn shape(input: &str) -> String {
        let lexed = lex(input).unwrap();
        let mut out = String::new();
        for t in &lexed.tokens {
            if t.spaced {
                out.push(' ');
            }
            match &t.kind {
                Tok::Or => out.push('|'),
                Tok::And => out.push('&'),
                Tok::Not => out.push('!'),
                Tok::Open => out.push('<'),
                Tok::Close => out.push('>'),
                Tok::Prefix(p) => out.push_str(&format!("P({})", p.spelling())),
                Tok::Text(t) => out.push_str(&format!(
                    "T({}{})",
                    String::from_utf8_lossy(&t.text),
                    if t.literal_wild { ",lit" } else { "" }
                )),
            }
        }
        out
    }

    #[test]
    fn operators_groups_and_text() {
        assert_eq!(shape("foo bar|ever"), "T(foo) T(bar)|T(ever)");
        assert_eq!(shape("foo | ever"), "T(foo) | T(ever)");
        assert_eq!(shape("<foo bar>|ever"), "<T(foo) T(bar)>|T(ever)");
        assert_eq!(shape("!foo|!<a b>"), "!T(foo)|!<T(a) T(b)>");
        assert_eq!(shape("foo ! ever"), "T(foo) ! T(ever)");
        assert_eq!(shape("<<spider>>"), "<<T(spider)>>");
        assert_eq!(shape("<>"), "<>");
        assert_eq!(shape("|"), "|");
    }

    /// `!` negates only where a term starts; elsewhere a name holds it.
    #[test]
    fn a_bang_inside_a_word_is_text() {
        assert_eq!(shape("wow!.txt"), "T(wow!.txt)");
        assert_eq!(shape("wow!"), "T(wow!)");
        assert_eq!(shape("!!foo"), "!!T(foo)");
        assert_eq!(shape("\"!bang\""), "T(!bang)");
    }

    #[test]
    fn quotes_keep_operators_literal_and_concatenate() {
        assert_eq!(shape("\"a|b\""), "T(a|b)");
        assert_eq!(shape("spider \"|\" man"), "T(spider) T(|) T(man)");
        assert_eq!(shape("\"spider\"man"), "T(spiderman)");
        assert_eq!(shape("spider\"man\""), "T(spiderman)");
        assert_eq!(shape("<\"foo|bar\">"), "<T(foo|bar)>");
        assert_eq!(shape("\"<a>\""), "T(<a>)");
        assert_eq!(shape("\"*.exe\""), "T(*.exe)");
    }

    #[test]
    fn keywords_are_operators_in_any_case() {
        assert_eq!(shape("foo and: bar"), "T(foo) & T(bar)");
        assert_eq!(shape("foo AND: bar"), "T(foo) & T(bar)");
        assert_eq!(shape("foo or:ever"), "T(foo) |T(ever)");
        assert_eq!(shape("NOT:foo"), "!T(foo)");
        assert_eq!(shape("not:not:foo"), "!!T(foo)");
        assert_eq!(shape("not: foo"), "! T(foo)");
        assert_eq!(shape("foo AND bar"), "T(foo) T(AND) T(bar)");
    }

    #[test]
    fn prefixes_and_function_values() {
        assert_eq!(shape("ext:pdf|txt"), "P(ext:)T(pdf)|T(txt)");
        assert_eq!(shape("size:>1mb"), "P(size:)T(>1mb)");
        assert_eq!(shape("<size:>1mb>"), "<P(size:)T(>1mb)>");
        assert_eq!(shape("dm:<2020"), "P(dm:)T(<2020)");
        assert_eq!(shape("size:>>bogus"), "P(size:)T(>>bogus)");
        assert_eq!(shape("Date-Modified:today"), "P(dm:)T(today)");
        assert_eq!(shape("ext:!pdf"), "P(ext:)!T(pdf)");
        assert_eq!(shape("size:!=1kb"), "P(size:)!T(=1kb)");
        assert_eq!(shape("ext:"), "P(ext:)T()");
        assert_eq!(shape("path:<words ever>"), "P(path:)<T(words) T(ever)>");
        assert_eq!(shape("file:!a"), "P(file:)!T(a)");
        assert_eq!(shape("file:path:words"), "P(file:)P(path:)T(words)");
        assert_eq!(shape("file:not:a"), "P(file:)!T(a)");
        assert_eq!(
            shape(r#"path:"C:\Program Files""#),
            r"P(path:)T(C:\Program Files)"
        );
        assert_eq!(shape("ext:path:x"), "P(ext:)T(path:x)");
        assert_eq!(
            shape("file:foo:bar"),
            "P(file:)T(foo:bar)",
            "a value is not checked"
        );
        assert_eq!(shape(r"C:\Windows 12:30"), r"T(C:\Windows) T(12:30)");
        assert_eq!(shape("\"a:b\" ?case:"), "T(a:b) T(?case:)");
    }

    #[test]
    fn directives_leave_no_token() {
        let lexed = lex("Report case: matchpath:1 x case:0").unwrap();
        assert_eq!(lexed.match_case, Some(false), "the last wins");
        assert_eq!(lexed.match_path, Some(true));
        assert_eq!(lexed.tokens.len(), 2);
        assert_eq!(lex("case:\"1\"").unwrap().match_case, Some(true));
        assert!(matches!(
            lex("case:2"),
            Err(QueryError::Malformed { what: "case:", .. })
        ));
        assert!(matches!(
            lex("matchpath:maybe"),
            Err(QueryError::Malformed {
                what: "matchpath:",
                ..
            })
        ));
        assert_eq!(
            shape("file:case:"),
            "P(file:)T(case:)",
            "only a term's first name"
        );
    }

    #[test]
    fn precedence_switches_mark_the_tokens_after_them() {
        let lexed = lex("a b|c ?and-precedence: d|e and-precedence: f ?OR-precedence: g").unwrap();
        let marks: Vec<bool> = lexed.tokens.iter().map(|t| t.and_first).collect();
        // a b | c d | e f g
        assert_eq!(
            marks,
            [false, false, false, false, true, true, true, true, false]
        );
    }

    #[test]
    fn no_bang_makes_a_terms_bang_literal() {
        assert_eq!(shape("no-!:!bang"), "T(!bang)");
        assert_eq!(shape("no!:!bang !x"), "T(!bang) !T(x)");
        assert_eq!(shape("no-!: !bang"), " !T(bang)");
        assert_eq!(shape("?no-!: !bang !x|!y"), " T(!bang) T(!x)|T(!y)");
        assert_eq!(shape("!no-!:!bang"), "!T(!bang)");
        assert_eq!(shape("no-!:ext:!pdf"), "P(ext:)T(!pdf)");
    }

    #[test]
    fn entities_are_literal_characters() {
        assert_eq!(shape("&excl:bang"), "T(!bang)");
        assert_eq!(shape("\"&excl:bang\""), "T(!bang)", "in quotes too");
        assert_eq!(
            shape("&#33:bang &#x21:bang &#X0021:bang"),
            "T(!bang) T(!bang) T(!bang)"
        );
        assert_eq!(shape("foo&sp:bar"), "T(foo bar)");
        assert_eq!(shape("spider&vert:man"), "T(spider|man)");
        assert_eq!(shape("w&lt:x&gt:"), "T(w<x>)");
        assert_eq!(shape("&quot:a&amp:b"), "T(\"a&b)");
        assert_eq!(shape("&ast:.exe"), "T(*.exe,lit)");
        assert_eq!(shape("*.exe&quest:"), "T(*.exe?,lit)");
        assert_eq!(
            shape("a&b &foo:bar &#: &#x: &#1114112:"),
            "T(a&b) T(&foo:bar) T(&#:) T(&#x:) T(&#1114112:)"
        );
        let surrogate = lex("&#xD800:").unwrap();
        let Tok::Text(t) = &surrogate.tokens[0].kind else {
            panic!()
        };
        assert_eq!(t.text, [0xED, 0xA0, 0x80], "a lone surrogate, as WTF-8");
    }

    /// A group never joins text: Everything's group expansion is refused
    /// with the column, as are value groups, which goz does not read yet.
    #[test]
    fn group_expansion_is_refused_with_its_column() {
        let at = |input: &str| match lex(input) {
            Err(QueryError::GroupExpansion { column }) => column,
            other => panic!("{input}: {other:?}"),
        };
        assert_eq!(at("gr<a|e>y"), 3);
        assert_eq!(at("a<b"), 2);
        assert_eq!(at("<spider>man"), 9);
        assert_eq!(at("<a><b>"), 4);
        assert_eq!(at("<a>!b"), 4);
        assert_eq!(at("x size:1<2"), 9);
        assert!(lex("<a>|<b> <c>").is_ok());
        assert!(matches!(
            lex("ext:<pdf|txt>"),
            Err(QueryError::Malformed { what: "ext:", .. })
        ));
    }

    #[test]
    fn columns_count_characters() {
        let lexed = lex("é <x").unwrap();
        let columns: Vec<usize> = lexed.tokens.iter().map(|t| t.column).collect();
        assert_eq!(columns, [1, 3, 4]);
    }
}
