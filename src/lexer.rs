// Lexical analysis

use serde::{Deserialize, Serialize};
use std::str::Chars;

/// TokenKind enumeration, including all possible Token types
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub enum TokenKind {
    // ==================== Literal ====================
    /// Integer: 123
    Int,
    /// Floating point: 1.5
    Float,
    /// String: "hello"
    Str,
    /// Template string: `template`
    Strs,
    /// Regular expression: /pattern/flags
    Regex,
    /// Identifiers (variable names, function names, etc.) and keywords are all here
    Identifier,

    // ==================== Arithmetic operators ====================
    /// Addition operator, corresponding to `+`
    Plus,
    /// Subtraction operator, corresponding `-`
    Minus,
    /// Multiplication operator, corresponding to `*`
    Multiply,
    /// Division operator, corresponding to `/`
    Divide,
    /// Modulo operator, corresponding to `%`
    Modulo,
    /// Power operator, corresponding to `**`
    Power,

    // ==================== Bitwise operators ====================
    /// Bitwise XOR operator, corresponding to `^`
    Xor,
    /// Bitwise AND operator, corresponding to `&`
    BitAnd,
    /// Bitwise OR operator, corresponding to `|`
    BitOr,
    /// Bitwise negation operator, corresponding to `~`
    BitNot,

    // ==================== Shift operator ====================
    /// Left shift operator, corresponding to `<<`
    ShiftLeft,
    /// Right shift operator, corresponding to `>>`
    ShiftRight,

    // ==================== Comparison operators ====================
    /// Less than operator, corresponding to `<`
    Less,
    /// Less than or equal to operator, corresponding to `<=`
    LessEqual,
    /// Greater than operator, corresponding to `>`
    Greater,
    /// Greater than or equal to operator, corresponding to `>=`
    GreaterEqual,

    // ==================== Equality operator ====================
    /// Equality operator, corresponding to `==`
    Equal,
    /// Strict equality operator, corresponding to `===`
    StrictEqual,
    /// Inequality operator, corresponding to `!=`
    NotEqual,
    /// Strict inequality operator, corresponding to `!==`
    StrictNotEqual,

    // ==================== Logical operators ====================
    /// Logical AND operator, corresponding to `&&`
    And,
    /// Logical OR operator, corresponding to `||`
    Or,
    /// Logical NOT operator, corresponding to `!`
    Not,

    // ==================== Assignment operator ====================
    /// Assignment operator, corresponding to `=`.
    Assign,

    // ==================== Other operators ====================
    /// Arrow operator, corresponding to `->`.
    Arrow,
    /// Match-branch arrow, corresponding to `=>`.
    FatArrow,
    /// Question mark operator, corresponds to `?`
    Question,
    /// Null value coalescing operator, corresponds to `??`
    Coalesce,
    /// Colon, corresponds to `:`
    Colon,
    /// Semicolon, corresponds to `;`
    Semicolon,
    /// Comma, corresponds to `,`
    Comma,
    /// Dot, corresponds `.`
    Dot,
    /// Range operator `..`, used by loops such as `for i in 1..100`.
    Range,
    /// File mode command start character, corresponding to `#`.
    ///
    /// The `source` module treats `#` as a file directive only at the first character
    /// of the first line. Elsewhere it remains an ordinary token, leaving room for
    /// precise diagnostics or future syntax.
    FileDirective,

    // ==================== Parentheses ====================
    /// Left parenthesis, corresponding to `(`
    LeftParen,
    /// Right parenthesis, corresponding to `)`
    RightParen,
    /// Left curly brace, corresponding to `{`
    LeftBrace,
    /// Right curly bracket, corresponding to `}`
    RightBrace,
    /// Left square bracket, corresponding to `[`
    LeftBracket,
    /// Right square bracket, corresponding to `]`
    RightBracket,

    // ====================== Increase and decrease ====================
    /// Increment operator, corresponds to `++`
    Increment,
    /// Decrement operator, corresponds to `--`
    Decrement,

    // ==================== Compound assignment operator ======================
    /// Post-addition assignment operator, corresponds to `+=`
    PlusAssign,
    /// Assignment operator after subtraction, corresponds to `-=`
    MinusAssign,
    /// Assignment operator after multiplication, corresponds to `*=`
    MultiplyAssign,
    /// Assignment operator after division, corresponds to `/=`
    DivideAssign,
    /// Assignment operator after modulo, corresponds to `%=`
    ModuloAssign,
    /// Assignment operator after left shift, corresponds to `<<=`
    ShiftLeftAssign,
    /// Assignment operator after right shift, corresponds to `>>=`
    ShiftRightAssign,
    /// Bitwise AND after assignment operator, corresponds to `&=`
    BitAndAssign,
    /// Bitwise XOR after assignment operator, corresponds to `^=`
    BitXorAssign,
    /// Bitwise OR post-assignment operator, corresponding to `|=`
    BitOrAssign,

    // ==================== Keywords ====================
    /// Module import keyword, corresponding to `use`
    Use,
    /// Variable declaration keyword, corresponding to `let`
    Let,
    /// Print output (without line breaks), corresponding to `print`
    Print,
    /// Print output (line breaks), corresponding `println`
    Println,
    /// Conditional judgment keyword, corresponding to `if`
    If,
    /// Otherwise, branch keyword, corresponding to `else`
    Else,
    /// Otherwise, if branch keyword, corresponding to `elseif`
    ElseIf,
    /// Function definition keyword, corresponding to `fn`
    Fn,
    /// Function return keyword, corresponding `return`
    Return,
    /// Class definition keyword, corresponding to `class`
    Class,
    /// Public access modifier keyword, corresponding to `pub`
    Pub,
    /// Instantiation object keyword, corresponding to `new`
    New,
    /// When loop keyword, corresponding to `while`
    While,
    /// Infinite loop keyword, corresponding `loop`
    Loop,
    /// Count loop keyword, corresponding to `for`
    For,
    /// Traversal operator keyword, corresponding to `in`
    In,
    /// Jump out of loop keyword, corresponding to `break`
    Break,
    /// Continue to the next loop keyword, corresponding to `continue`
    Continue,
    /// Error catching keyword, corresponding to `try`
    Try,
    /// Error catching branch keyword, corresponding to `catch`
    Catch,
    /// Throwing error keyword, corresponding to `throw`
    Throw,
    /// Pattern matching keyword, corresponding to `match`
    Match,

    // ==================== Special Token ====================
    /// Newline character, used as statement separator
    Wrap,
    /// End of file, marking the end of source code
    Eof,
}

/// Token data generated by lexical analysis.
///
/// Token only saves the type and source code location, and does not hold source code slices to avoid copying text in the lexical stage.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct Token {
    /// The type of Token, the parser only performs syntactic dispatch based on the type.
    pub kind: TokenKind,
    /// The byte length of Token in the source code.
    pub len: u32,
    /// The starting byte position of Token in the source code.
    pub start: usize,
    /// The end byte position of Token in the source code.
    pub end: usize,
    /// The line number of the starting position of Token, starting from 1.
    pub line: usize,
    /// The column number of the starting position of Token, starting from 1 and counting in Unicode characters.
    pub column: usize,
}

impl Token {
    /// Creates a token without a source location, for internal sentinels such as EOF.
    fn new(kind: TokenKind, len: u32) -> Self {
        Token {
            kind,
            len,
            start: 0,
            end: len as usize,
            line: 1,
            column: 1,
        }
    }

    /// Creates a token with a source location.
    ///
    /// Only integer positions are stored; source text is not copied on the lexer hot path.
    fn with_pos(
        kind: TokenKind,
        len: u32,
        start: usize,
        end: usize,
        line: usize,
        column: usize,
    ) -> Self {
        Token {
            kind,
            len,
            start,
            end,
            line,
            column,
        }
    }
}

// ============================================================
// Cursor adopts Rust's official solution
// ============================================================

const EOF_CHAR: char = '\0';

/// Cursor that scans source characters while tracking the current token location.
pub struct Cursor<'a> {
    /// Original source text, used to check keywords or regular context by token position.
    input: &'a str,
    /// The number of remaining bytes from the starting point of the current token to the end of the source code.
    len_remaining: usize,
    /// Current character iterator, responsible for advancing the source code by Unicode characters.
    chars: Chars<'a>,
    /// The row number of the current cursor, starting from 1.
    line: usize,
    /// The column number where the current cursor is located, starting from 1 and counting in Unicode characters.
    column: usize,
    /// The starting byte position of the current token.
    token_start: usize,
    /// The line number of the starting position of the current token.
    token_line: usize,
    /// The column number of the starting position of the current token.
    token_column: usize,
}

impl<'a> Cursor<'a> {
    /// Creates a new lexical scan cursor.
    ///
    /// The cursor only borrows the source code string and does not copy the input content. Subsequent tokens refer to the original source code through byte positions.
    fn new(input: &'a str) -> Self {
        Cursor {
            input,
            len_remaining: input.len(),
            chars: input.chars(),
            line: 1,
            column: 1,
            token_start: 0,
            token_line: 1,
            token_column: 1,
        }
    }

    /// Peeks at the next character.
    ///
    /// Returns an internal EOF sentinel at the end, keeping callers' match logic simple.
    fn first(&self) -> char {
        self.chars.clone().next().unwrap_or(EOF_CHAR)
    }

    /// Peeks two characters ahead.
    ///
    /// This method only clones the character iterator for pre-reading and does not advance the actual cursor position.
    fn second(&self) -> char {
        let mut iter = self.chars.clone();
        iter.next();
        iter.next().unwrap_or(EOF_CHAR)
    }

    /// Determines whether the cursor has been scanned to the end of the source code.
    fn is_eof(&self) -> bool {
        self.chars.as_str().is_empty()
    }

    /// Returns the number of bytes consumed by the current token.
    ///
    /// The value comes from the change in remaining bytes, avoiding an extra source
    /// slice or token-text allocation.
    fn pos_within_token(&self) -> u32 {
        (self.len_remaining - self.chars.as_str().len()) as u32
    }

    /// Sets the current cursor position to the starting point of the next token.
    ///
    /// Records the byte offset, line, and column for the next token.
    fn reset_pos_within_token(&mut self) {
        self.len_remaining = self.chars.as_str().len();
        self.token_start = self.input.len() - self.chars.as_str().len();
        self.token_line = self.line;
        self.token_column = self.column;
    }

    /// Consumes a character and updates the line and column.
    ///
    /// A newline advances the line and resets the column to 1; other characters only advance the column.
    fn bump(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(c)
    }

    /// Consumes characters while the predicate matches.
    ///
    /// The caller passes in a lightweight predicate used to skip whitespace, read identifiers, or read regular flag bits.
    fn eat_while(&mut self, mut predicate: impl FnMut(char) -> bool) {
        while predicate(self.first()) && !self.is_eof() {
            self.bump();
        }
    }

    /// Consumes characters until the given ASCII byte or end of input.
    ///
    /// Used for single-line comments; the ASCII delimiter keeps matching inexpensive.
    fn eat_until(&mut self, byte: u8) {
        while !self.is_eof() && self.first() != byte as char {
            self.bump();
        }
    }
}

// ============================================================
// Character classification
// ============================================================

/// Determines whether the character is a blank character that needs to be skipped or folded by the BT lexical layer.
fn is_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

/// Returns whether a character may begin an identifier.
fn is_id_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}

/// Determine whether the character can be used as the subsequent character of the identifier.
fn is_id_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

// ============================================================
// Lexical analysis main function
// ============================================================

/// Converts source text into a lazy token iterator.
///
/// The iterator advances the cursor on demand and does not allocate the token list at once; EOF is only used as an internal end mark and is not returned externally.
pub fn tokenize(input: &str) -> impl Iterator<Item = Token> + '_ {
    let mut cursor = Cursor::new(input);
    std::iter::from_fn(move || {
        let token = cursor.advance_token();
        if token.kind != TokenKind::Eof {
            Some(token)
        } else {
            None
        }
    })
}

// ============================================================
// advance_token - core logic
// ============================================================

impl Cursor<'_> {
    /// Scans and returns the next token.
    ///
    /// This lexer hot path handles whitespace, comments, keywords, operators, and
    /// literals directly in one match, avoiding extra abstraction overhead.
    pub fn advance_token(&mut self) -> Token {
        use TokenKind::*;

        self.reset_pos_within_token();

        let Some(first_char) = self.bump() else {
            return Token::new(Eof, 0);
        };

        let token_kind = match first_char {
            // Blank
            c if is_whitespace(c) => {
                // Preserve a statement boundary when the whitespace run contains LF, including
                // Windows CRLF. The previous look-ahead ran after consuming the full run and
                // therefore lost CRLF line breaks whose first character was CR.
                let mut has_line_feed = c == '\n';
                while is_whitespace(self.first()) {
                    has_line_feed |= self.bump() == Some('\n');
                }
                if has_line_feed {
                    Wrap
                } else {
                    self.reset_pos_within_token();
                    return self.advance_token();
                }
            }

            // Single-line comment
            '/' if self.first() == '/' => {
                self.eat_until(b'\n');
                self.reset_pos_within_token();
                return self.advance_token();
            }

            // Multi-line comment
            '/' if self.first() == '*' => {
                self.bump();
                loop {
                    match self.bump() {
                        None => break,
                        Some('*') if self.first() == '/' => {
                            self.bump();
                            break;
                        }
                        _ => {}
                    }
                }
                self.reset_pos_within_token();
                return self.advance_token();
            }

            // Identifier (regardless of keywords, always return Identifier)
            c if is_id_start(c) => {
                self.eat_while(is_id_continue);
                let len = self.pos_within_token() as usize;
                let start = self.input.len() - self.len_remaining;
                let text = &self.input[start..start + len];
                match text {
                    "use" => Use,
                    "let" => Let,
                    "print" => Print,
                    "println" => Println,
                    "if" => If,
                    "else" => Else,
                    "elseif" => ElseIf,
                    "fn" => Fn,
                    "return" => Return,
                    "class" => Class,
                    "pub" => Pub,
                    "new" => New,
                    "while" => While,
                    "loop" => Loop,
                    "for" => For,
                    "in" => In,
                    "break" => Break,
                    "continue" => Continue,
                    "try" => Try,
                    "catch" => Catch,
                    "throw" => Throw,
                    "match" => Match,
                    _ => Identifier,
                }
            }

            // Number
            '0'..='9' => self.number(),

            // Dot
            '.' => {
                if self.first() == '.' {
                    self.bump();
                    Range
                } else if self.first().is_ascii_digit() {
                    self.number()
                } else {
                    Dot
                }
            }

            // Minus sign
            '-' => match self.first() {
                '-' => {
                    self.bump();
                    Decrement
                }
                '=' => {
                    self.bump();
                    MinusAssign
                }
                '>' => {
                    self.bump();
                    Arrow
                }
                _ => Minus,
            },

            // Plus sign
            '+' => match self.first() {
                '+' => {
                    self.bump();
                    Increment
                }
                '=' => {
                    self.bump();
                    PlusAssign
                }
                _ => Plus,
            },

            // Asterisk
            '*' => match self.first() {
                '*' => {
                    self.bump();
                    Power
                }
                '=' => {
                    self.bump();
                    MultiplyAssign
                }
                _ => Multiply,
            },

            // Percent sign
            '%' => {
                if self.first() == '=' {
                    self.bump();
                    ModuloAssign
                } else {
                    Modulo
                }
            }

            // Exclusive OR
            '^' => {
                if self.first() == '=' {
                    self.bump();
                    BitXorAssign
                } else {
                    Xor
                }
            }

            // Bitwise negation
            '~' => BitNot,

            // Bitwise AND / Logical AND
            '&' => match self.first() {
                '&' => {
                    self.bump();
                    And
                }
                '=' => {
                    self.bump();
                    BitAndAssign
                }
                _ => BitAnd,
            },

            // Bitwise OR / Logical OR
            '|' => match self.first() {
                '|' => {
                    self.bump();
                    Or
                }
                '=' => {
                    self.bump();
                    BitOrAssign
                }
                _ => BitOr,
            },

            // is less than
            '<' => match self.first() {
                '=' => {
                    self.bump();
                    LessEqual
                }
                '<' => {
                    self.bump();
                    if self.first() == '=' {
                        self.bump();
                        ShiftLeftAssign
                    } else {
                        ShiftLeft
                    }
                }
                _ => Less,
            },

            // is greater than
            '>' => match self.first() {
                '=' => {
                    self.bump();
                    GreaterEqual
                }
                '>' => {
                    self.bump();
                    if self.first() == '=' {
                        self.bump();
                        ShiftRightAssign
                    } else {
                        ShiftRight
                    }
                }
                _ => Greater,
            },

            // Equal sign
            '=' => match self.first() {
                '>' => {
                    self.bump();
                    FatArrow
                }
                '=' => {
                    self.bump();
                    if self.first() == '=' {
                        self.bump();
                        StrictEqual
                    } else {
                        Equal
                    }
                }
                _ => Assign,
            },

            // Question mark
            '?' => {
                if self.first() == '?' {
                    self.bump();
                    Coalesce
                } else {
                    Question
                }
            }

            // Exclamation mark
            '!' => match self.first() {
                '=' => {
                    self.bump();
                    if self.first() == '=' {
                        self.bump();
                        StrictNotEqual
                    } else {
                        NotEqual
                    }
                }
                _ => Not,
            },

            // Division sign / Regular
            '/' => self.regex_or_divide(),

            // Single character
            ':' => Colon,
            ';' => Semicolon,
            ',' => Comma,
            '(' => LeftParen,
            ')' => RightParen,
            '{' => LeftBrace,
            '}' => RightBrace,
            '[' => LeftBracket,
            ']' => RightBracket,
            '#' => FileDirective,

            // String
            '"' | '\'' => self.string(first_char),
            '`' => self.template_string(),

            // Illegal characters: skip
            _ => {
                self.reset_pos_within_token();
                return self.advance_token();
            }
        };

        let len = self.pos_within_token();
        let start = self.token_start;
        let end = self.input.len() - self.chars.as_str().len();
        let line = self.token_line;
        let column = self.token_column;
        self.reset_pos_within_token();
        Token::with_pos(token_kind, len, start, end, line, column)
    }

    /// Scan numeric literals and differentiate between integers and floating point numbers.
    ///
    /// Supports `_` separators. A dot begins a fractional part only when followed by
    /// a digit, so property access remains unambiguous.
    fn number(&mut self) -> TokenKind {
        use TokenKind::*;
        let mut is_float = false;

        loop {
            match self.first() {
                '0'..='9' => {
                    self.bump();
                }
                '_' => {
                    self.bump();
                }
                '.' if !is_float && self.second().is_ascii_digit() => {
                    is_float = true;
                    self.bump();
                }
                _ => break,
            }
        }

        if is_float {
            Float
        } else {
            Int
        }
    }

    /// Scans ordinary string literals.
    ///
    /// `quote` is the opening delimiter. Escaped characters are skipped so an escaped
    /// quote cannot terminate the literal early.
    fn string(&mut self, quote: char) -> TokenKind {
        use TokenKind::*;
        loop {
            match self.bump() {
                None => break,
                Some(c) if c == quote => break,
                Some('\\') => {
                    self.bump();
                }
                _ => {}
            }
        }
        Str
    }

    /// Scans template string literals.
    ///
    /// The template string is wrapped in backticks, and backslash escaping is also supported. The specific template semantics are left to the subsequent parsing stage.
    fn template_string(&mut self) -> TokenKind {
        use TokenKind::*;
        loop {
            match self.bump() {
                None => break,
                Some('`') => break,
                Some('\\') => {
                    self.bump();
                }
                _ => {}
            }
        }
        Strs
    }

    /// Distinguishes a regex literal from division using the preceding context.
    ///
    /// Only attempts to read the regular expression at the beginning of the expression; otherwise, the division sign is returned directly to avoid misjudgment of ordinary division as a regular expression.
    fn regex_or_divide(&mut self) -> TokenKind {
        use TokenKind::*;
        if self.first() == '=' {
            self.bump();
            return DivideAssign;
        }
        let previous = self.input[..self.token_start]
            .chars()
            .rev()
            .find(|ch| !matches!(ch, ' ' | '\t' | '\r'));
        let can_start_regex = previous.is_none_or(|ch| {
            matches!(
                ch,
                '(' | '[' | '{' | '=' | ':' | ',' | '?' | '!' | '&' | '|' | ';' | '\n'
            )
        });
        if !can_start_regex {
            return Divide;
        }

        let mut escaped = false;
        loop {
            match self.bump() {
                None | Some('\n') => return Divide,
                Some('/') if !escaped => break,
                Some('\\') if !escaped => escaped = true,
                Some(_) => escaped = false,
            }
        }
        self.eat_while(|ch| ch.is_ascii_alphabetic());
        Regex
    }
}
