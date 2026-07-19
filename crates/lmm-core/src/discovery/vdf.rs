//! Minimal parser for Valve's text KeyValues format (VDF/ACF), enough for
//! `libraryfolders.vdf` and `appmanifest_*.acf`.
//!
//! Grammar: a document is a sequence of `key value` pairs where key is a
//! (usually quoted) string and value is either a string or a `{ ... }` block.
//! `//` starts a line comment. Duplicate keys are preserved.

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Block(Vec<(String, Value)>),
}

impl Value {
    /// First value for a key (case-insensitive, as Valve's own parsers are).
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Block(pairs) => pairs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            Value::Str(_) => None,
        }
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Value::Str(s) => Some(s),
            Value::Block(_) => None,
        }
    }

    pub fn pairs(&self) -> &[(String, Value)] {
        match self {
            Value::Block(pairs) => pairs,
            Value::Str(_) => &[],
        }
    }
}

#[derive(Debug, PartialEq)]
enum Token {
    Str(String),
    Open,
    Close,
}

struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
}

impl Lexer<'_> {
    fn next_token(&mut self) -> Result<Option<Token>, String> {
        loop {
            match self.chars.peek() {
                None => return Ok(None),
                Some(c) if c.is_whitespace() => {
                    self.chars.next();
                }
                Some('/') => {
                    // Only "//" line comments exist in VDF.
                    self.chars.next();
                    if self.chars.peek() == Some(&'/') {
                        for c in self.chars.by_ref() {
                            if c == '\n' {
                                break;
                            }
                        }
                    } else {
                        return Err("stray '/'".into());
                    }
                }
                Some('{') => {
                    self.chars.next();
                    return Ok(Some(Token::Open));
                }
                Some('}') => {
                    self.chars.next();
                    return Ok(Some(Token::Close));
                }
                Some('"') => {
                    self.chars.next();
                    let mut s = String::new();
                    loop {
                        match self.chars.next() {
                            None => return Err("unterminated string".into()),
                            Some('"') => break,
                            Some('\\') => match self.chars.next() {
                                Some('n') => s.push('\n'),
                                Some('t') => s.push('\t'),
                                Some(c) => s.push(c), // \" \\ and anything else literal
                                None => return Err("unterminated escape".into()),
                            },
                            Some(c) => s.push(c),
                        }
                    }
                    return Ok(Some(Token::Str(s)));
                }
                // Unquoted token (allowed by the format, rare in practice).
                Some(_) => {
                    let mut s = String::new();
                    while let Some(&c) = self.chars.peek() {
                        if c.is_whitespace() || c == '{' || c == '}' || c == '"' {
                            break;
                        }
                        s.push(c);
                        self.chars.next();
                    }
                    return Ok(Some(Token::Str(s)));
                }
            }
        }
    }
}

/// Parse a whole document into an implicit root block.
pub fn parse(text: &str) -> Result<Value, String> {
    let mut lexer = Lexer {
        chars: text.chars().peekable(),
    };
    let mut root = Vec::new();
    parse_pairs(&mut lexer, &mut root, true)?;
    Ok(Value::Block(root))
}

fn parse_pairs(
    lexer: &mut Lexer,
    out: &mut Vec<(String, Value)>,
    top_level: bool,
) -> Result<(), String> {
    loop {
        let key = match lexer.next_token()? {
            None if top_level => return Ok(()),
            None => return Err("unexpected end of file inside block".into()),
            Some(Token::Close) if !top_level => return Ok(()),
            Some(Token::Str(s)) => s,
            Some(t) => return Err(format!("expected key, got {t:?}")),
        };
        match lexer.next_token()? {
            Some(Token::Str(s)) => out.push((key, Value::Str(s))),
            Some(Token::Open) => {
                let mut inner = Vec::new();
                parse_pairs(lexer, &mut inner, false)?;
                out.push((key, Value::Block(inner)));
            }
            _ => return Err(format!("expected value for key '{key}'")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_libraryfolders_shape() {
        let doc = parse(
            r#"
"libraryfolders"
{
    // a comment
    "0"
    {
        "path"      "/home/user/.local/share/Steam"
        "label"     ""
        "apps" { "70" "12345" "220" "999" }
    }
    "1"
    {
        "path"      "/mnt/games/SteamLibrary"
    }
}
"#,
        )
        .unwrap();
        let folders = doc.get("libraryfolders").unwrap();
        assert_eq!(
            folders.get("0").unwrap().get_str("path"),
            Some("/home/user/.local/share/Steam")
        );
        assert_eq!(
            folders.get("1").unwrap().get_str("path"),
            Some("/mnt/games/SteamLibrary")
        );
        assert_eq!(
            folders.get("0").unwrap().get("apps").unwrap().pairs().len(),
            2
        );
    }

    #[test]
    fn parses_escapes_and_unquoted() {
        let doc = parse("key \"a\\\"b\\\\c\"\nplain value").unwrap();
        assert_eq!(doc.get_str("key"), Some("a\"b\\c"));
        assert_eq!(doc.get_str("plain"), Some("value"));
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse("\"unterminated").is_err());
        assert!(parse("\"k\" {").is_err());
        assert!(parse("\"k\"").is_err());
    }
}
