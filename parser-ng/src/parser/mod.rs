/*! This module contains a handwritten [PEG][1] parser for YARA rules.

The parser receives a sequence of tokens produced by the [`Tokenizer`], and
produces a Concrete Syntax-Tree ([`CST`]), also known as a lossless syntax
tree. The CST is initially represented as a stream of [events][`Event`], but
this stream is later converted to a tree using the [rowan][2] create.

This parser is error-tolerant, it is able to parse YARA code that contains
syntax errors. After each error, the parser recovers and keeps parsing the
remaining code. The resulting CST may contain error nodes containing portions
of the code that are not syntactically correct, but anything outside of those
error nodes is valid YARA code.

[1]: https://en.wikipedia.org/wiki/Parsing_expression_grammar
[2]: https://github.com/rust-analyzer/rowan
 */

use indexmap::{IndexMap, IndexSet};
use rustc_hash::{FxHashMap, FxHashSet};

#[cfg(feature = "logging")]
use log::*;

pub mod cst;

mod token_stream;

#[cfg(test)]
mod tests;

use crate::parser::cst::{Event, SyntaxKind, SyntaxStream};
use crate::parser::token_stream::TokenStream;
use crate::tokenizer::{Token, TokenId, Tokenizer};
use crate::Span;

/// Produces a Concrete Syntax-Tree ([`CST`]) for a given YARA source code.
pub struct Parser<'src> {
    parser: ParserImpl<'src>,
    whitespaces: bool,
}

impl<'src> Parser<'src> {
    /// Creates a new parser for the given source code.
    pub fn new(source: &'src [u8]) -> Self {
        Self {
            parser: ParserImpl::from(Tokenizer::new(source)),
            whitespaces: true,
        }
    }

    /// Enables or disables whitespaces in the returned CST.
    ///
    /// If false, the resulting CST won't contain whitespaces.
    ///
    /// Default value is `true`.
    pub fn whitespaces(mut self, yes: bool) -> Self {
        self.whitespaces = yes;
        self
    }

    /// Returns the source code passed to the parser.
    #[inline]
    pub fn source(&self) -> &'src [u8] {
        self.parser.tokens.source()
    }

    /// Returns the CST as a sequence of events.
    #[inline]
    pub fn events(self) -> Events<'src> {
        Events { parser: self.parser, whitespaces: self.whitespaces }
    }

    /// Consumes the parser and builds a Concrete Syntax Tree (CST).
    #[inline]
    pub fn build_cst(self) -> CST {
        CST::from(self)
    }
}

/// An CST in the form of a sequence of [`events`][`Event`].
pub struct Events<'src> {
    parser: ParserImpl<'src>,
    whitespaces: bool,
}

impl<'src> Iterator for Events<'src> {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        if self.whitespaces {
            self.parser.next()
        } else {
            loop {
                match self.parser.next()? {
                    // ignore whitespace and get next event
                    Event::Token { kind: WHITESPACE, .. } => {}
                    token => break Some(token),
                }
            }
        }
    }
}

/// Describes the state of the parser.
enum ParserState {
    /// Indicates that the parser is as the start of the input.
    StartOfInput,
    /// Indicates that the parser is at the end of the input.
    EndOfInput,
    /// The parser is OK, it can continue parsing.
    OK,
    /// The parser has failed to parse some portion of the source code. It can
    /// recover from the failure and go back to OK.
    Failure,
}

/// Internal implementation of the parser. The [`Parser`] type is only a
/// wrapper around this type.
struct ParserImpl<'src> {
    /// Stream from where the parser consumes the input tokens.
    tokens: TokenStream<'src>,

    /// Stream where the parser puts the events that conform the resulting CST.
    output: SyntaxStream,

    /// The current state of the parser.
    state: ParserState,

    /// How deep is the parser into "optional" branches of the grammar. An
    /// optional branch is one that can fail without the whole production
    /// rule failing. For instance, in `A := B? C` the parser can fail while
    /// parsing `B`, but this failure is acceptable because `B` is optional.
    /// Less obvious cases of optional branches are present in alternatives
    /// and the "zero or more" operation (examples: `(A|B)`, `A*`).
    opt_depth: usize,

    /// How deep is the parse into "not" branches of the grammar.
    not_depth: usize,

    /// How deep is the parser into grammar branches.
    #[cfg(feature = "logging")]
    depth: usize,

    /// Hash map where keys are spans within the source code, and values
    /// are a list of tokens that were expected to match at that span.
    ///
    /// This hash map plays a crucial role in error reporting during parsing.
    /// Consider the following grammar rule:
    ///
    /// `A := a? b`
    ///
    /// Here, the optional token `a` must be followed by the token `b`. This
    /// can be represented (conceptually, not actual code) as:
    ///
    /// ```text
    /// self.start(A)
    ///     .opt(|p| p.expect(a))
    ///     .expect(b)
    ///     .end()
    /// ```
    ///
    /// If we attempt to parse the sequence `cb`, it will fail at `c` because
    /// the rule matches only `ab` and `b`. The error message should be:
    ///
    /// "expecting `a` or `b`, found `c`"
    ///
    /// This error is generated by the `expect(b)` statement. However, the
    /// `expect` function only knows about the `b` token. So, how do we know
    /// that both `a` and `b` are valid tokens at the position where `c` was
    /// found?
    ///
    /// This is where the `expected_token_errors` hash map comes into play. We
    /// know that `a` is also a valid alternative because the `expect(a)`
    /// inside the `opt` was tried and failed. The parser doesn't fail at that
    /// point because `a` is optional, but it records that `a` was expected at
    /// the position of `c`. When `expect(b)` fails later, the parser looks up
    /// any other token (besides `b`) that were expected to match at the
    /// position and produces a comprehensive error message.
    expected_token_errors: FxHashMap<Span, IndexSet<&'static str>>,

    /// Similar to `expected_token_errors` but tracks the positions where
    /// unexpected tokens were found. This type of error is produced when
    /// [`ParserImpl::not`] is used. This only stores the span were the
    /// unexpected token was found.
    unexpected_token_errors: FxHashSet<Span>,

    /// Errors that are not yet sent to the `output` stream. The purpose of
    /// this map is removing duplicate messages for the same code span. In
    /// certain cases the parser can produce two different error messages for
    /// the same span, but this map guarantees that only the first error is
    /// taken into account and that any further error for the same span is
    /// ignored.
    pending_errors: IndexMap<Span, String>,

    /// A cache for storing partial parser results. Each item in the set is a
    /// (position, SyntaxKind) tuple, where position is the absolute index
    /// of a token withing the source code. The presence of a tuple in the
    /// cache indicates that the non-terminal indicated by SyntaxKind failed
    /// to match that position. Notice that only parser failures are cached,
    /// but successes are not cached. [packrat][1] parsers usually cache both
    /// failure and successes, but we cache only failures because this enough
    /// for speeding up some edge cases, while memory consumption remains low
    /// because we don't need to store the actual result of the parser, only
    /// the fact that if failed.
    ///
    /// [1]: https://en.wikipedia.org/wiki/Packrat_parser
    cache: FxHashSet<(usize, SyntaxKind)>,
}

impl<'src> From<Tokenizer<'src>> for ParserImpl<'src> {
    /// Creates a new parser that receives tokens from the given [`Tokenizer`].
    fn from(tokenizer: Tokenizer<'src>) -> Self {
        Self {
            tokens: TokenStream::new(tokenizer),
            output: SyntaxStream::new(),
            pending_errors: IndexMap::new(),
            expected_token_errors: FxHashMap::default(),
            unexpected_token_errors: FxHashSet::default(),
            cache: FxHashSet::default(),
            opt_depth: 0,
            not_depth: 0,
            #[cfg(feature = "logging")]
            depth: 0,
            state: ParserState::StartOfInput,
        }
    }
}

/// The parser behaves as an iterator that returns events of type [`Event`].
impl Iterator for ParserImpl<'_> {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        match self.state {
            ParserState::StartOfInput => {
                self.state = ParserState::OK;
                Some(Event::Begin(SOURCE_FILE))
            }
            ParserState::EndOfInput => None,
            _ => {
                // If the output buffer isn't empty, return a buffered event.
                if let Some(token) = self.output.pop() {
                    return Some(token);
                }
                // If the output buffer is empty and there are pending tokens, invoke
                // the parser to consume tokens and put more events in the output
                // buffer.
                //
                // Each call to `next` parses one top-level item (either an import
                // statement or rule declaration). This approach parses the source
                // code lazily, one top-level item at a time, saving memory by
                // avoiding tokenizing the entire input at once, or producing all
                // the events before they are consumed.
                if self.tokens.has_more() {
                    let _ = self.trivia();
                    let _ = self.top_level_item();
                    self.flush_errors();
                    self.cache.clear();
                    self.state = ParserState::OK;
                }
                // If still there are no more tokens, we have reached the end of
                // the input.
                if let Some(token) = self.output.pop() {
                    Some(token)
                } else {
                    self.state = ParserState::EndOfInput;
                    Some(Event::End(SOURCE_FILE))
                }
            }
        }
    }
}

/// Parser private API.
///
/// This section contains utility functions that are used by the grammar rules.
impl<'src> ParserImpl<'src> {
    /// Returns the next token, without consuming it.
    ///
    /// Returns `None` if there are no more tokens.
    fn peek(&mut self) -> Option<&Token> {
        self.tokens.peek_token(0)
    }

    /// Returns the next non-trivia token, without consuming any token.
    ///
    /// Trivia tokens are those that are not really relevant and can be ignored,
    /// like whitespaces, newlines, and comments. This function skips trivia
    /// tokens until finding one that is non-trivia.
    fn peek_non_ws(&mut self) -> Option<&Token> {
        let mut i = 0;
        // First find the position of the first token that is not a whitespace
        // and then use `peek_token` again for returning it. This is necessary
        // due to a current limitation in the borrow checker that doesn't allow
        // this:
        //
        // loop {
        //     match self.tokens.peek_token(i) {
        //         Some(token) => {
        //             if token.is_trivia() {
        //                 i += 1;
        //             } else {
        //                 return Some(token);
        //             }
        //         }
        //         None => return None,
        //     }
        // }
        //
        let token_pos = loop {
            match self.tokens.peek_token(i) {
                Some(token) => {
                    if token.is_trivia() {
                        i += 1;
                    } else {
                        break i;
                    }
                }
                None => return None,
            }
        };
        self.tokens.peek_token(token_pos)
    }

    /// Consumes the next token and returns it. The consumed token is also
    /// appended to the output.
    ///
    /// Returns `None` if there are no more tokens.
    fn bump(&mut self) -> Option<Token> {
        let token = self.tokens.next_token();
        match &token {
            Some(token) => self.output.push_token(token.into(), token.span()),
            None => {}
        }
        token
    }

    /// Sets a bookmark at the current parser state.
    ///
    /// This saves the current parser state, allowing the parser to try
    /// a grammar production, and if it fails, go back to the saved state
    /// and try a different grammar production.
    fn bookmark(&mut self) -> Bookmark {
        Bookmark {
            tokens: self.tokens.bookmark(),
            output: self.output.bookmark(),
        }
    }

    /// Restores the parser to the state indicated by the bookmark.
    fn restore_bookmark(&mut self, bookmark: &Bookmark) {
        self.tokens.restore_bookmark(&bookmark.tokens);
        self.output.truncate(&bookmark.output);
    }

    /// Removes a bookmark.
    ///
    /// Once a bookmark is removed the parser can't be restored to the
    /// state indicated by the bookmark.
    fn remove_bookmark(&mut self, bookmark: Bookmark) {
        self.tokens.remove_bookmark(bookmark.tokens);
        self.output.remove_bookmark(bookmark.output);
    }

    /// Switches to hex pattern mode.
    fn enter_hex_pattern_mode(&mut self) -> &mut Self {
        if matches!(self.state, ParserState::Failure) {
            return self;
        }
        self.tokens.enter_hex_pattern_mode();
        self
    }

    /// Switches to hex jump mode.
    fn enter_hex_jump_mode(&mut self) -> &mut Self {
        if matches!(self.state, ParserState::Failure) {
            return self;
        }
        self.tokens.enter_hex_jump_mode();
        self
    }

    /// Indicates the start of a non-terminal symbol of a given kind.
    ///
    /// Must be followed by a matching [`Parser::end`].
    fn begin(&mut self, kind: SyntaxKind) -> &mut Self {
        self.trivia();

        #[cfg(feature = "logging")]
        {
            debug!(
                "{}{:?}    -- next token: {}",
                "  ".repeat(self.depth),
                kind,
                self.tokens
                    .peek_token(0)
                    .map(|t| format!("{:?}", t))
                    .unwrap_or_default()
            );
            self.depth += 1;
        }

        self.output.begin(kind);
        self
    }

    /// Indicates the end of the non-terminal symbol that was previously
    /// started with [`Parser::begin`].
    fn end(&mut self) -> &mut Self {
        #[cfg(feature = "logging")]
        {
            self.depth -= 1;
        }

        if matches!(self.state, ParserState::Failure) {
            self.output.end_with_error();
        } else {
            self.output.end();
        }
        self
    }

    fn recover(&mut self) -> &mut Self {
        self.state = ParserState::OK;
        self
    }

    fn sync(&mut self, recovery_set: &'static TokenSet) -> &mut Self {
        self.trivia();
        match self.peek() {
            None => return self,
            Some(token) => {
                if recovery_set.contains(token).is_some() {
                    return self;
                } else {
                    let span = token.span();
                    self.expected_token_errors
                        .entry(span)
                        .or_default()
                        .extend(
                            recovery_set
                                .token_ids()
                                .map(|token| token.description()),
                        );
                    if self.pending_errors.is_empty() {
                        self.handle_errors();
                    } else {
                        self.flush_errors();
                    }
                }
            }
        }
        self.output.begin(ERROR);
        while let Some(token) = self.peek() {
            if recovery_set.contains(token).is_some() {
                break;
            } else {
                self.bump();
            }
        }
        self.output.end();
        self
    }

    /// Recovers the parser from a previous error, consuming any token that is
    /// not in the recovery set and putting them under an error node in the
    /// resulting tree.
    ///
    /// The purpose of this function is establishing a point for the parser to
    /// recover from parsing errors. For instance, consider the following
    /// grammar rules:
    ///
    /// ```text
    /// A := aBC
    /// B := bb
    /// C := ccc
    /// ```
    ///
    /// `A` is roughly expressed as:
    ///
    /// ```text
    /// self.begin(A)
    ///     .expect(a)
    ///     .one(|p| p.B())
    ///     .one(|p| p.C())
    ///     .end()
    /// ```
    ///
    /// Suppose that we are parsing the sequence `axxc`. The sequence starts
    /// with `a`, so `expect(a)` is successful. However, `one(|p| p.B())`
    /// fails because `x` is found instead of the expected `b`. As a result,
    /// `one(|p| p.C())` is not attempted, and the entire `A` production fails,
    /// resulting a CST that looks like:
    ///
    /// ```text
    /// error
    ///   a
    ///   x
    ///   x
    ///   c
    /// ```
    ///
    /// By inserting `recover_and_sync(c)`, we can recover from previous errors
    /// before trying to match `C`:
    ///
    /// ```text
    /// self.begin(A)
    ///     .expect(a)
    ///     .one(|p| p.B())
    ///     .recover_and_sync(c)
    ///     .one(|p| p.C())
    ///     .end()
    /// ```
    ///
    /// If the parser fails at `one(|p| p.B())`, leaving the `xx` tokens
    /// unconsumed, `recover_and_sync(c)` will consume them until it finds a
    /// `c` token and will recover from the error. This allows `one(|p| p.C())`
    /// to consume the `c` and succeed. The resulting CST would be like:
    ///
    /// ```text
    /// A
    ///   a
    ///   error
    ///     x
    ///     x
    ///   c
    /// ```
    ///
    /// Notice how the error is now more localized.
    fn recover_and_sync(
        &mut self,
        recovery_set: &'static TokenSet,
    ) -> &mut Self {
        self.recover();
        self.sync(recovery_set);
        self
    }

    /// Consumes trivia tokens until finding one that is non-trivia.
    ///
    /// Trivia tokens those that are not really part of the language, like
    /// whitespaces, newlines and comments.
    fn trivia(&mut self) -> &mut Self {
        if matches!(self.state, ParserState::Failure) {
            return self;
        }
        while let Some(token) = self.peek() {
            if token.is_trivia() {
                self.bump();
            } else {
                break;
            }
        }
        self
    }

    /// Checks that the next non-trivia token matches one of the expected
    /// tokens.
    ///
    /// If the next non-trivia token does not match any of the expected tokens,
    /// no token will be consumed, the parser will transition to a failure
    /// state and generate an error message. If it matches, the non-trivia
    /// token and any trivia token that appears in front of it will be
    /// consumed and sent to the output.
    ///
    /// # Panics
    ///
    /// If `expected_tokens` is empty.
    fn expect(&mut self, expected_tokens: &'static TokenSet) -> &mut Self {
        self.expect_d(expected_tokens, None)
    }

    /// Like [`ParserImpl::expect`], but allows specifying a custom
    /// description for the expected tokens.
    fn expect_d(
        &mut self,
        expected_tokens: &'static TokenSet,
        description: Option<&'static str>,
    ) -> &mut Self {
        assert!(!expected_tokens.is_empty());

        if matches!(self.state, ParserState::Failure) {
            return self;
        }

        let found_expected_token = match self.peek_non_ws() {
            None => None,
            Some(token) => {
                let span = token.span();
                let token = expected_tokens.contains(token);

                match (self.not_depth, token) {
                    // The expected token was found, but we are inside a "not".
                    // When we are inside a "not", any "expect" is negated, and
                    // actually means that the token was *not* expected.
                    (not_depth, Some(_)) if not_depth > 0 => {
                        self.unexpected_token_errors.insert(span);
                        self.handle_errors()
                    }
                    // We are not inside a "not", and the expected token was
                    // not found.
                    (0, None) => {
                        let tokens = self
                            .expected_token_errors
                            .entry(span.clone())
                            .or_default();

                        if let Some(description) = description {
                            tokens.insert(description);
                        } else {
                            tokens.extend(
                                expected_tokens
                                    .token_ids()
                                    .map(|token| token.description()),
                            );
                        }

                        self.handle_errors();
                    }
                    _ => {}
                }

                token
            }
        };

        if let Some(t) = found_expected_token {
            // Consume any trivia token in front of the non-trivia expected
            // token.
            self.trivia();
            // Consume the expected token.
            let token = self.tokens.next_token().unwrap();
            self.output.push_token(*t, token.span());
            // After matching a token that is not inside an "optional" branch
            // in the grammar, it's guaranteed that the parser won't go back
            // to a position at the left of the matched token. This is a good
            // opportunity for flushing errors.
            if self.opt_depth == 0 {
                self.flush_errors()
            }
        } else {
            self.state = ParserState::Failure;
        }

        self
    }

    /// Begins an alternative.
    ///
    /// # Example
    ///
    /// ```text
    /// p.begin_alt()
    ///   .alt(..)
    ///   .alt(..)
    ///  .end_alt()
    /// ```
    fn begin_alt(&mut self) -> Alt<'_, 'src> {
        let bookmark = self.bookmark();
        Alt { parser: self, matched: false, bookmark }
    }

    /// Applies `parser` optionally.
    ///
    /// If `parser` fails, the failure is ignored and the parser is reset to
    /// its previous state.
    ///
    /// # Example
    ///
    /// ```text
    /// p.opt(|p| p.something_optional())
    /// ```
    fn opt<P>(&mut self, parser: P) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        if matches!(self.state, ParserState::Failure) {
            return self;
        }

        let bookmark = self.bookmark();

        self.trivia();
        self.opt_depth += 1;
        parser(self);
        self.opt_depth -= 1;

        // Any error occurred while parsing the optional production is ignored.
        if matches!(self.state, ParserState::Failure) {
            self.recover();
            self.restore_bookmark(&bookmark);
        }

        self.remove_bookmark(bookmark);
        self
    }

    /// Negates the result of `parser`.
    ///
    /// If `parser` is successful the parser transitions to failure state.
    fn not<P>(&mut self, parser: P) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        if matches!(self.state, ParserState::Failure) {
            return self;
        }

        let bookmark = self.bookmark();

        self.trivia();

        self.not_depth += 1;
        parser(self);
        self.not_depth -= 1;

        self.state = match self.state {
            ParserState::OK => ParserState::Failure,
            ParserState::Failure => ParserState::OK,
            _ => unreachable!(),
        };

        self.restore_bookmark(&bookmark);
        self.remove_bookmark(bookmark);
        self
    }

    /// Like [`ParserImpl::expect`], but optional.
    fn opt_expect(&mut self, expected_tokens: &'static TokenSet) -> &mut Self {
        self.opt(|p| p.expect(expected_tokens))
    }

    /// If the next non-trivia token matches one of the expected tokens,
    /// consume all trivia tokens and applies `parser`.
    ///
    /// `if_next(TOKEN, |p| p.expect(TOKEN))` is logically equivalent to
    /// `opt(|p| p.expect(TOKEN))`, but the former is more efficient because it
    /// doesn't do any backtracking. The closure `|p| p.expect(TOKEN)` is
    /// executed only after we are sure that the next non-trivia token is
    /// `TOKEN`.
    ///
    /// This can be used for replacing `opt` when the optional production can
    /// be unequivocally distinguished by its first token. For instance, in a
    /// YARA rule the metadata section is optional, but always starts with
    /// the `meta` keyword, so, instead of:
    ///
    /// `opt(|p| p.meta_blk()`)
    ///
    /// We can use:
    ///
    /// `if_next(t!(META_KW), |p| p.meta_blk())`
    ///
    fn if_next<P>(
        &mut self,
        expected_tokens: &'static TokenSet,
        parser: P,
    ) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        if matches!(self.state, ParserState::Failure) {
            return self;
        }
        match self.peek_non_ws() {
            None => {}
            Some(token) => {
                if expected_tokens.contains(token).is_some() {
                    self.trivia();
                    parser(self);
                } else {
                    let span = token.span();
                    self.expected_token_errors
                        .entry(span)
                        .or_default()
                        .extend(
                            expected_tokens
                                .token_ids()
                                .map(|t| t.description()),
                        );
                }
            }
        }
        self
    }

    /// If the next non-trivia token matches one of the expected tokens,
    /// consume all trivia tokens, consume the expected token, and applies
    /// `parser`.
    ///
    /// This is similar to [`ParserImpl::if_next`], the difference between
    /// both functions reside on how they handle the expected token. `if_next`
    /// leave the expected token in the stream, to be consumed by `parser`,
    /// while `cond` consumes the expected token too.
    fn cond<P>(
        &mut self,
        expected_tokens: &'static TokenSet,
        parser: P,
    ) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        self.if_next(expected_tokens, |p| {
            p.expect(expected_tokens).then(|p| parser(p))
        });
        self
    }

    /// Applies `parser` zero or more times.
    #[inline]
    fn zero_or_more<P>(&mut self, parser: P) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        self.n_or_more(0, parser)
    }

    /// Applies `parser` one or more times.
    #[inline]
    fn one_or_more<P>(&mut self, parser: P) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        self.n_or_more(1, parser)
    }

    /// Applies `parser` N or more times.
    fn n_or_more<P>(&mut self, n: usize, parser: P) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        if matches!(self.state, ParserState::Failure) {
            return self;
        }
        // The first N times that `f` is called it must match.
        for _ in 0..n {
            self.trivia();
            parser(self);
            if matches!(self.state, ParserState::Failure) {
                return self;
            }
        }
        // If the first N matches were ok, keep matching `f` as much as
        // possible.
        loop {
            let bookmark = self.bookmark();
            self.trivia();
            self.opt_depth += 1;
            parser(self);
            self.opt_depth -= 1;
            if matches!(self.state, ParserState::Failure) {
                self.recover();
                self.restore_bookmark(&bookmark);
                self.remove_bookmark(bookmark);
                break;
            } else {
                self.remove_bookmark(bookmark);
            }
        }
        self
    }

    /// Applies `parser` exactly one time.
    fn then<P>(&mut self, parser: P) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        if matches!(self.state, ParserState::Failure) {
            return self;
        }
        self.trivia();
        parser(self);
        self
    }

    fn cached<P>(&mut self, kind: SyntaxKind, parser: P) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        let start_index = self.tokens.current_token_index();

        if self.cache.contains(&(start_index, kind)) {
            self.state = ParserState::Failure;
            return self;
        }

        parser(self);

        if matches!(self.state, ParserState::Failure) {
            self.cache.insert((start_index, kind));
        }

        self
    }

    fn flush_errors(&mut self) {
        self.expected_token_errors.clear();
        for (span, error) in self.pending_errors.drain(0..) {
            self.output.push_error(error, span);
        }
    }

    fn handle_errors(&mut self) {
        if self.opt_depth > 0 {
            return;
        }

        // From all errors in expected_token_errors, use the one at the largest
        // offset. If several errors start at the same offset, the last one is
        // used.
        let expected_token = self
            .expected_token_errors
            .drain()
            .max_by_key(|(span, _)| span.start());

        // From all errors in unexpected_token_errors, use the one at the
        // largest offset. If several errors start at the same offset, the last
        // one is used.
        let unexpected_token = self
            .unexpected_token_errors
            .drain()
            .max_by_key(|span| span.start());

        let (span, expected) = match (expected_token, unexpected_token) {
            (Some((e, _)), Some(u)) if u.start() > e.start() => (u, None),
            (None, Some(u)) => (u, None),
            (Some((e, expected)), _) => (e, Some(expected)),
            _ => unreachable!(),
        };

        // There's a previous error for the same span, ignore this one.
        if self.pending_errors.contains_key(&span) {
            return;
        }

        let actual_token = String::from_utf8_lossy(
            self.tokens.source().get(span.range()).unwrap(),
        );

        let error_msg = if let Some(expected) = expected {
            let (last, all_except_last) =
                expected.as_slice().split_last().unwrap();

            if all_except_last.is_empty() {
                format!("expecting {last}, found `{actual_token}`")
            } else {
                format!(
                    "expecting {} or {last}, found `{actual_token}`",
                    itertools::join(all_except_last.iter(), ", "),
                )
            }
        } else {
            format!("unexpected `{actual_token}`")
        };

        self.pending_errors.insert(span, error_msg);
    }
}

use crate::cst::{syntax_stream, CST};
use SyntaxKind::*;

macro_rules! t {
    ($( $tokens:path )|*) => {
       &TokenSet(&[$( $tokens ),*])
    };
}

/// Grammar rules.
///
/// Each function in this section parses a piece of YARA source code. For
/// instance, the `import_stmt` function parses a YARA import statement,
/// `rule_decl` parses a rule declaration, etc. Usually, each function is
/// associated to a non-terminal symbol in the grammar, and the function's
/// code defines the grammar production rule for that symbol.
///
/// Let's use the following grammar rule as an example:
///
/// ```text
/// A := a B (C | D)
/// ```
///
/// `A`, `B`, `C` and `D` are non-terminal symbols, while `a` is a terminal
/// symbol (or token). This rule can be read: `A` is expanded as the token
/// `a` followed by the non-terminal symbol `B`, followed by either `C` or
/// `D`.
///
/// This rule would be expressed as:
///
/// ```text
/// fn A(&mut self) -> &mut Self {
///   self.begin(SyntaxKind::A)
///       .expect(t!(a))
///       .one(|p| p.B())
///       .begin_alt()
///          .alt(|p| p.C())
///          .alt(|p| p.D())
///       .end_alt()
///       .end()
/// }
/// ```
///
/// Also notice the use of `begin_alt` and `end_alt` for enclosing alternatives
/// like `(C | D)`. In PEG parsers the order of alternatives is important, the
/// parser tries them sequentially and accepts the first successful match.
/// Thus, a rule like `( a | a B )` is problematic because `a B` won't ever
/// match. If `a B` matches, then `a` also matches, but `a` has a higher
/// priority and prevents `a B` from matching.
impl<'src> ParserImpl<'src> {
    /// Parses a top-level item in YARA source file.
    ///
    /// A top-level item is either an import statement or a rule declaration.
    ///
    /// ```text
    /// TOP_LEVEL_ITEM ::= ( IMPORT_STMT | RULE_DECL )
    /// ```
    fn top_level_item(&mut self) -> &mut Self {
        let token = match self.peek() {
            Some(token) => token,
            None => {
                self.state = ParserState::Failure;
                return self;
            }
        };
        match token {
            Token::IMPORT_KW(_) => self.import_stmt(),
            Token::GLOBAL_KW(_) | Token::PRIVATE_KW(_) | Token::RULE_KW(_) => {
                self.rule_decl()
            }
            token => {
                let span = token.span();
                let token_str = token.description();
                self.output.push_error(
                    format!("expecting import statement or rule definition, found {}", token_str),
                    span,
                );
                self.output.begin(ERROR);
                self.bump();
                self.output.end();
                self.state = ParserState::Failure;
                self
            }
        }
    }

    /// Parses an import statement.
    ///
    /// ```text
    /// IMPORT_STMT ::= `import` STRING_LIT
    /// ```
    fn import_stmt(&mut self) -> &mut Self {
        self.begin(IMPORT_STMT)
            .expect(t!(IMPORT_KW))
            .expect(t!(STRING_LIT))
            .end()
    }

    /// Parses a rule declaration.
    ///
    /// ```text
    /// RULE_DECL ::= RULE_MODS? `rule` IDENT `{`
    ///   META_BLK?
    ///   PATTERNS_BLK?
    ///   CONDITION_BLK
    /// `}`
    /// ```
    fn rule_decl(&mut self) -> &mut Self {
        self.begin(RULE_DECL)
            .opt(|p| p.rule_mods())
            .expect(t!(RULE_KW))
            .expect(t!(IDENT))
            .if_next(t!(COLON), |p| p.rule_tags())
            .recover_and_sync(t!(L_BRACE))
            .expect(t!(L_BRACE))
            .recover_and_sync(t!(META_KW | STRINGS_KW | CONDITION_KW))
            .if_next(t!(META_KW), |p| p.meta_blk())
            .recover_and_sync(t!(STRINGS_KW | CONDITION_KW))
            .if_next(t!(STRINGS_KW), |p| p.patterns_blk())
            .recover_and_sync(t!(CONDITION_KW))
            .condition_blk()
            .recover_and_sync(t!(R_BRACE))
            .expect(t!(R_BRACE))
            .end()
    }

    /// Parses rule modifiers.
    ///
    /// ```text
    /// RULE_MODS := ( `private` `global`? | `global` `private`? )
    /// ```
    fn rule_mods(&mut self) -> &mut Self {
        self.begin(RULE_MODS)
            .begin_alt()
            .alt(|p| p.expect(t!(PRIVATE_KW)).opt_expect(t!(GLOBAL_KW)))
            .alt(|p| p.expect(t!(GLOBAL_KW)).opt_expect(t!(PRIVATE_KW)))
            .end_alt()
            .end()
    }

    /// Parsers rule tags.
    ///
    /// ```text
    /// RULE_TAGS := `:` IDENT+
    /// ```
    fn rule_tags(&mut self) -> &mut Self {
        self.begin(RULE_TAGS)
            .expect(t!(COLON))
            .one_or_more(|p| p.expect(t!(IDENT)))
            .end()
    }

    /// Parses metadata block.
    ///
    /// ```text
    /// META_BLK := `meta` `:` META_DEF+
    /// ``
    fn meta_blk(&mut self) -> &mut Self {
        self.begin(META_BLK)
            .expect(t!(META_KW))
            .expect(t!(COLON))
            .one_or_more(|p| p.meta_def())
            /*.then(|p| {
                while matches!(p.peek_non_ws(), Some(IDENT(_))) {
                    p.trivia();
                    p.meta_def();
                    p.recover_and_sync(t!(IDENT | STRINGS_KW | CONDITION_KW));
                }
                p
            })*/
            .end()
    }

    /// Parses a metadata definition.
    ///
    /// ```text
    /// META_DEF := IDENT `=` (
    ///     `true`      |
    ///     `false`     |
    ///     INTEGER_LIT |
    ///     FLOAT_LIT   |
    ///     STRING_LIT
    /// )
    /// ``
    fn meta_def(&mut self) -> &mut Self {
        self.begin(META_DEF)
            .expect(t!(IDENT))
            .expect(t!(EQUAL))
            .begin_alt()
            .alt(|p| {
                p.opt_expect(t!(MINUS)).expect(t!(INTEGER_LIT | FLOAT_LIT))
            })
            .alt(|p| p.expect(t!(STRING_LIT | TRUE_KW | FALSE_KW)))
            .end_alt()
            .end()
    }

    /// Parses the patterns block.
    ///
    /// ```text
    /// PATTERNS_BLK := `strings` `:` PATTERN_DEF+
    /// ``
    fn patterns_blk(&mut self) -> &mut Self {
        self.begin(PATTERNS_BLK)
            .expect(t!(STRINGS_KW))
            .expect(t!(COLON))
            .one_or_more(|p| p.pattern_def())
            .end()
    }

    /// Parses a pattern definition.
    ///
    /// ```text
    /// PATTERN_DEF := PATTERN_IDENT `=` (
    ///     STRING_LIT  |
    ///     REGEXP      |
    ///     HEX_PATTERN
    /// )
    /// ``
    fn pattern_def(&mut self) -> &mut Self {
        self.begin(PATTERN_DEF)
            .expect(t!(PATTERN_IDENT))
            .expect(t!(EQUAL))
            .begin_alt()
            .alt(|p| p.expect(t!(STRING_LIT)))
            .alt(|p| p.expect(t!(REGEXP)))
            .alt(|p| p.hex_pattern())
            .end_alt()
            .opt(|p| p.pattern_mods())
            .end()
    }

    /// Parses pattern modifiers.
    ///
    /// ```text
    /// PATTERN_MODS := PATTERN_MOD+
    /// ``
    fn pattern_mods(&mut self) -> &mut Self {
        self.begin(PATTERN_MODS).one_or_more(|p| p.pattern_mod()).end()
    }

    /// Parses a pattern modifier.
    ///
    /// ```text
    /// PATTERN_MOD := (
    ///   `ascii`                                                  |
    ///   `wide`                                                   |
    ///   `nocase`                                                 |
    ///   `private`                                                |
    ///   `fullword`                                               |
    ///   `base64` | `base64wide` ( `(` STRING_LIT `)` )?          |
    ///   `xor` (
    ///       `(`
    ///         INTEGER_LIT ( `-` INTEGER_LIT) )?
    ///       `)`
    ///    )?
    /// )
    /// ``
    fn pattern_mod(&mut self) -> &mut Self {
        const DESC: Option<&'static str> = Some("pattern modifier");

        self.begin(PATTERN_MOD)
            .begin_alt()
            .alt(|p| {
                p.expect_d(
                    t!(ASCII_KW
                        | WIDE_KW
                        | NOCASE_KW
                        | PRIVATE_KW
                        | FULLWORD_KW),
                    DESC,
                )
            })
            .alt(|p| {
                p.expect_d(t!(BASE64_KW | BASE64WIDE_KW), DESC).opt(|p| {
                    p.expect(t!(L_PAREN))
                        .expect(t!(STRING_LIT))
                        .expect(t!(R_PAREN))
                })
            })
            .alt(|p| {
                p.expect_d(t!(XOR_KW), DESC).opt(|p| {
                    p.expect(t!(L_PAREN))
                        .expect(t!(INTEGER_LIT))
                        .opt(|p| p.expect(t!(HYPHEN)).expect(t!(INTEGER_LIT)))
                        .expect(t!(R_PAREN))
                })
            })
            .end_alt()
            .end()
    }

    /// Parses the condition block.
    ///
    /// ```text
    /// CONDITION_BLK := `condition` `:` BOOLEAN_EXPR
    /// ``
    fn condition_blk(&mut self) -> &mut Self {
        self.begin(CONDITION_BLK)
            .expect(t!(CONDITION_KW))
            .expect(t!(COLON))
            .then(|p| p.boolean_expr())
            .end()
    }

    /// Parses the condition block.
    ///
    /// ```text
    /// HEX_PATTERN := `{` HEX_SUB_PATTERN `}`
    /// ``
    fn hex_pattern(&mut self) -> &mut Self {
        self.begin(HEX_PATTERN)
            .expect(t!(L_BRACE))
            .enter_hex_pattern_mode()
            .then(|p| p.hex_sub_pattern())
            .expect(t!(R_BRACE))
            .end()
    }

    /// Parses the condition block.
    ///
    /// ```text
    /// HEX_SUB_PATTERN :=
    ///   (HEX_BYTE | HEX_ALTERNATIVE) (HEX_JUMP* (HEX_BYTE | HEX_ALTERNATIVE))*
    /// ``
    fn hex_sub_pattern(&mut self) -> &mut Self {
        self.begin(HEX_SUB_PATTERN)
            .begin_alt()
            .alt(|p| p.expect(t!(HEX_BYTE)))
            .alt(|p| p.hex_alternative())
            .end_alt()
            .zero_or_more(|p| {
                p.zero_or_more(|p| p.hex_jump())
                    .begin_alt()
                    .alt(|p| p.expect(t!(HEX_BYTE)))
                    .alt(|p| p.hex_alternative())
                    .end_alt()
            })
            .end()
    }

    /// Parses a hex pattern alternative.
    ///
    /// ```text
    /// HEX_ALTERNATIVE := `(` HEX_SUB_PATTERN ( `|` HEX_SUB_PATTERN )* `)`
    /// ``
    fn hex_alternative(&mut self) -> &mut Self {
        self.begin(HEX_ALTERNATIVE)
            .expect(t!(L_PAREN))
            .then(|p| p.hex_sub_pattern())
            .zero_or_more(|p| p.expect(t!(PIPE)).then(|p| p.hex_sub_pattern()))
            .expect(t!(R_PAREN))
            .end()
    }

    /// Parses a hex jump
    ///
    /// ```text
    /// HEX_JUMP := `[` ( INTEGER_LIT? `-` INTEGER_LIT? | INTEGER_LIT ) `]`
    /// ``
    fn hex_jump(&mut self) -> &mut Self {
        self.begin(HEX_JUMP)
            .expect(t!(L_BRACKET))
            .enter_hex_jump_mode()
            .begin_alt()
            .alt(|p| {
                p.opt_expect(t!(INTEGER_LIT))
                    .expect(t!(HYPHEN))
                    .opt_expect(t!(INTEGER_LIT))
            })
            .alt(|p| p.expect(t!(INTEGER_LIT)))
            .end_alt()
            .expect(t!(R_BRACKET))
            .end()
    }

    /// Parses a boolean expression.
    ///
    /// ```text
    /// BOOLEAN_EXPR := BOOLEAN_TERM ((AND_KW | OR_KW) BOOLEAN_TERM)*
    /// ``
    fn boolean_expr(&mut self) -> &mut Self {
        self.begin(BOOLEAN_EXPR)
            .boolean_term()
            .zero_or_more(|p| {
                p.expect(t!(AND_KW | OR_KW)).then(|p| p.boolean_term())
            })
            .end()
    }

    /// Parses a boolean term.
    ///
    /// ```text
    /// BOOLEAN_TERM := (
    ///    `true`                 |
    ///    `false`                |
    ///    `not` BOOLEAN_TERM     |
    ///    `defined` BOOLEAN_TERM |
    ///    `(` BOOLEAN_EXPR `)`
    /// )
    /// ``
    fn boolean_term(&mut self) -> &mut Self {
        self.begin(BOOLEAN_TERM)
            .begin_alt()
            .alt(|p| {
                p.expect(t!(PATTERN_IDENT))
                    .cond(t!(AT_KW), |p| p.expr())
                    .cond(t!(IN_KW), |p| p.range())
            })
            .alt(|p| p.expect(t!(TRUE_KW | FALSE_KW)))
            .alt(|p| {
                p.expect(t!(NOT_KW | DEFINED_KW)).then(|p| p.boolean_term())
            })
            .alt(|p| p.for_expr())
            .alt(|p| p.of_expr())
            .alt(|p| {
                p.expr().zero_or_more(|p| {
                    p.expect(t!(EQ
                        | NE
                        | LE
                        | LT
                        | GE
                        | GT
                        | CONTAINS_KW
                        | ICONTAINS_KW
                        | STARTSWITH_KW
                        | ISTARTSWITH_KW
                        | ENDSWITH_KW
                        | IENDSWITH_KW
                        | MATCHES_KW))
                        .then(|p| p.expr())
                })
            })
            .alt(|p| {
                p.expect(t!(L_PAREN))
                    .then(|p| p.boolean_expr())
                    .expect(t!(R_PAREN))
            })
            .end_alt()
            .end()
    }

    /// Parses an expression.
    ///
    /// ```text
    /// EXPR := (
    ///    TERM  ( (arithmetic_op | bitwise_op | `.`) TERM)*
    /// )
    /// ``
    fn expr(&mut self) -> &mut Self {
        self.begin(EXPR)
            .term()
            .zero_or_more(|p| {
                p.expect(t!(ADD
                    | SUB
                    | MUL
                    | DIV
                    | MOD
                    | SHL
                    | SHR
                    | BITWISE_AND
                    | BITWISE_OR
                    | BITWISE_XOR
                    | BITWISE_NOT
                    | DOT))
                    .then(|p| p.term())
            })
            .end()
    }

    /// Parses a term.
    ///
    /// ```text
    /// TERM := (
    ///     indexing_expr   |
    ///     func_call_expr  |
    ///     primary_expr    |
    /// )
    /// ``
    fn term(&mut self) -> &mut Self {
        self.begin(TERM)
            .then(|p| p.primary_expr())
            .cond(t!(L_BRACKET), |p| p.expr().expect(t!(R_BRACKET)))
            .cond(t!(L_PAREN), |p| {
                p.opt(|p| p.boolean_expr())
                    .zero_or_more(|p| {
                        p.expect(t!(COMMA)).then(|p| p.boolean_expr())
                    })
                    .expect(t!(R_PAREN))
            })
            .end()
    }

    /// Parses a range.
    ///
    /// ```text
    /// RANGE := `(` EXPR `.` `.` EXPR `)`
    /// ``
    fn range(&mut self) -> &mut Self {
        self.begin(RANGE)
            .expect(t!(L_PAREN))
            .then(|p| p.expr())
            .expect(t!(DOT))
            .expect(t!(DOT))
            .then(|p| p.expr())
            .expect(t!(R_PAREN))
            .end()
    }

    /// Parsers a primary expression.
    ///
    /// ```text
    /// PRIMARY_EXPR := (
    ///     FLOAT_LIT                          |
    ///     INTEGER_LIT                        |
    ///     STRING_LIT                         |
    ///     REGEXP                             |
    ///     `filesize`                         |
    ///     `entrypoint`                       |
    ///     PATTERN_COUNT (`in` RANGE)?        |
    ///     PATTERN_OFFSET (`[` EXPR `]`)?     |
    ///     PATTERN_LENGTH (`[` EXPR `]`)?     |
    ///     `-` TERM                           |
    ///     `~` TERM                           |
    ///     `(` EXPR `)`                       |
    ///     IDENT (`.` IDENT)*
    /// )
    /// ``
    fn primary_expr(&mut self) -> &mut Self {
        const DESC: Option<&'static str> = Some("expression");

        self.cached(PRIMARY_EXPR, |p| {
            p.begin(PRIMARY_EXPR)
                .begin_alt()
                .alt(|p| {
                    p.expect_d(
                        t!(FLOAT_LIT
                            | INTEGER_LIT
                            | STRING_LIT
                            | REGEXP
                            | FILESIZE_KW
                            | ENTRYPOINT_KW),
                        DESC,
                    )
                })
                .alt(|p| {
                    p.expect_d(t!(PATTERN_COUNT), DESC)
                        .opt(|p| p.expect(t!(IN_KW)).then(|p| p.range()))
                })
                .alt(|p| {
                    p.expect_d(t!(PATTERN_OFFSET | PATTERN_LENGTH), DESC).opt(
                        |p| {
                            p.expect(t!(L_BRACKET))
                                .then(|p| p.expr())
                                .expect(t!(R_BRACKET))
                        },
                    )
                })
                .alt(|p| p.expect_d(t!(MINUS), DESC).then(|p| p.term()))
                .alt(|p| p.expect_d(t!(BITWISE_NOT), DESC).then(|p| p.term()))
                .alt(|p| {
                    p.expect_d(t!(L_PAREN), DESC)
                        .then(|p| p.expr())
                        .expect(t!(R_PAREN))
                })
                .alt(|p| {
                    p.expect_d(t!(IDENT), DESC)
                        .zero_or_more(|p| p.expect(t!(DOT)).expect(t!(IDENT)))
                })
                .end_alt()
                .end()
        })
    }

    /// Parses `for` expression.
    ///
    /// ```text
    /// FOR_EXPR := `for` QUANTIFIER (
    ///     `of` ( `them` | PATTERN_IDENT_TUPLE ) |
    ///     IDENT ( `,` IDENT )* `in` ITERABLE
    /// )
    /// `:` `(` BOOLEAN_EXPR `)
    /// ``
    fn for_expr(&mut self) -> &mut Self {
        self.begin(FOR_EXPR)
            .expect(t!(FOR_KW))
            .then(|p| p.quantifier())
            .begin_alt()
            .alt(|p| {
                p.expect(t!(OF_KW))
                    .begin_alt()
                    .alt(|p| p.expect(t!(THEM_KW)))
                    .alt(|p| p.pattern_ident_tuple())
                    .end_alt()
            })
            .alt(|p| {
                p.expect(t!(IDENT))
                    .zero_or_more(|p| p.expect(t!(COMMA)).expect(t!(IDENT)))
                    .expect(t!(IN_KW))
                    .then(|p| p.iterable())
            })
            .end_alt()
            .expect(t!(COLON))
            .expect(t!(L_PAREN))
            .then(|p| p.boolean_expr())
            .expect(t!(R_PAREN))
            .end()
    }

    /// Parses `of` expression.
    ///
    /// ```text
    /// OF := QUANTIFIER (
    ///     `of` ( `them` | PATTERN_IDENT_TUPLE ) ( `at` EXPR | `in` RANGE )? |
    ///     BOOLEAN_EXPR_TUPLE
    /// )
    /// ``
    fn of_expr(&mut self) -> &mut Self {
        self.begin(OF_EXPR)
            .then(|p| p.quantifier())
            .expect(t!(OF_KW))
            .begin_alt()
            .alt(|p| {
                p.begin_alt()
                    .alt(|p| p.expect(t!(THEM_KW)))
                    .alt(|p| p.pattern_ident_tuple())
                    .end_alt()
                    .cond(t!(AT_KW), |p| p.expr())
                    .cond(t!(IN_KW), |p| p.range())
            })
            .alt(|p| {
                p.boolean_expr_tuple().not(|p| p.expect(t!(AT_KW | IN_KW)))
            })
            .end_alt()
            .end()
    }

    /// Parses quantifier.
    ///
    /// ```text
    /// QUANTIFIER := (
    ///     `all`                           |
    ///     `none`                          |
    ///     `any`                           |
    ///     (INTEGER_LIT | FLOAT_LIT ) `%`  |
    ///     EXPR !`%`
    /// )
    /// ```
    fn quantifier(&mut self) -> &mut Self {
        self.begin(QUANTIFIER)
            .begin_alt()
            .alt(|p| p.expect(t!(ALL_KW | NONE_KW | ANY_KW)))
            // Quantifier can be either a primary expression followed by a %,
            // or an expression not followed by %. We can't make it an expression
            // followed by an optional % because that leads to ambiguity, as
            // expressions can contain the % operator (mod).
            .alt(|p| p.primary_expr().expect(t!(PERCENT)))
            .alt(|p| p.expr().not(|p| p.expect(t!(PERCENT))))
            .end_alt()
            .end()
    }

    /// Parses iterable.
    ///
    /// ```text
    /// ITERABLE := (
    ///     RANGE              |
    ///     EXPR_TUPLE         |
    ///     EXPR
    /// )
    /// ```
    fn iterable(&mut self) -> &mut Self {
        self.begin(ITERABLE)
            .begin_alt()
            .alt(|p| p.range())
            .alt(|p| p.expr_tuple())
            .alt(|p| p.expr())
            .end_alt()
            .end()
    }

    /// Parses a tuple of boolean expressions.
    ///
    /// ```text
    /// BOOLEAN_EXPR_TUPLE := `(` BOOLEAN_EXPR ( `,` BOOLEAN_EXPR )* `)`
    /// ```
    fn boolean_expr_tuple(&mut self) -> &mut Self {
        self.begin(BOOLEAN_EXPR_TUPLE)
            .expect(t!(L_PAREN))
            .then(|p| p.boolean_expr())
            .zero_or_more(|p| p.expect(t!(COMMA)).then(|p| p.boolean_expr()))
            .expect(t!(R_PAREN))
            .end()
    }

    /// Parses a tuple of expressions.
    ///
    /// ```text
    /// EXPR_TUPLE := `(` EXPR ( `,` EXPR )* `)`
    /// ```
    fn expr_tuple(&mut self) -> &mut Self {
        self.begin(EXPR_TUPLE)
            .expect(t!(L_PAREN))
            .then(|p| p.expr())
            .zero_or_more(|p| p.expect(t!(COMMA)).then(|p| p.expr()))
            .expect(t!(R_PAREN))
            .end()
    }

    /// Parses a tuple of pattern identifiers.
    ///
    /// ```text
    /// PATTERN_IDENT_TUPLE := `(` PATTERN_IDENT `*`? ( `,` PATTERN_IDENT `*`? )* `)`
    /// ```
    fn pattern_ident_tuple(&mut self) -> &mut Self {
        self.begin(PATTERN_IDENT_TUPLE)
            .expect(t!(L_PAREN))
            .expect(t!(PATTERN_IDENT))
            .opt_expect(t!(ASTERISK)) // TODO white spaces between ident and *
            .zero_or_more(|p| {
                p.expect(t!(COMMA))
                    .expect(t!(PATTERN_IDENT))
                    .opt_expect(t!(ASTERISK))
            })
            .expect(t!(R_PAREN))
            .end()
    }
}

struct Bookmark {
    tokens: token_stream::Bookmark,
    output: syntax_stream::Bookmark,
}

/// A set of tokens passed to the [`ParserImpl::expect`]
/// function.
///
/// The set is represented by a list of [`SyntaxKind`].
struct TokenSet(&'static [SyntaxKind]);

impl TokenSet {
    #[inline]
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// If the set contains the give `token`, returns `Some` with the
    /// [`SyntaxKind`] that corresponds to the matching token. Otherwise, it
    /// returns `None`.
    fn contains(&self, token: &Token) -> Option<&SyntaxKind> {
        self.0.iter().find(|t| t.token_id() == token.id())
    }

    /// Returns the token IDs associated to the tokens in the set.
    fn token_ids(&self) -> impl Iterator<Item = TokenId> + 'static {
        self.0.iter().map(move |t| t.token_id())
    }
}

struct Alt<'a, 'src> {
    parser: &'a mut ParserImpl<'src>,
    matched: bool,
    bookmark: Bookmark,
}

impl<'a, 'src> Alt<'a, 'src> {
    fn alt<F>(mut self, f: F) -> Self
    where
        F: Fn(&'a mut ParserImpl<'src>) -> &'a mut ParserImpl<'src>,
    {
        if matches!(self.parser.state, ParserState::Failure) {
            return self;
        }
        // Don't try to match the current alternative if the parser a previous
        // one already matched.
        if !self.matched {
            self.parser.trivia();
            self.parser.opt_depth += 1;
            self.parser = f(self.parser);
            self.parser.opt_depth -= 1;
            match self.parser.state {
                // The current alternative matched.
                ParserState::OK => {
                    self.matched = true;
                }
                // The current alternative didn't match, restore the token
                // stream to the position it has before trying to match.
                ParserState::Failure => {
                    self.parser.recover();
                    self.parser.restore_bookmark(&self.bookmark);
                }
                _ => unreachable!(),
            };
        }
        self
    }

    fn end_alt(self) -> &'a mut ParserImpl<'src> {
        self.parser.remove_bookmark(self.bookmark);
        // If none of the alternatives matched, that's a failure.
        if self.matched {
            self.parser.state = ParserState::OK;
        } else {
            self.parser.state = ParserState::Failure;
            self.parser.handle_errors();
        };
        self.parser
    }
}