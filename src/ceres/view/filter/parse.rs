use super::{Filter, FilterParseError, Selector, ViewPath, canonical::is_safe};

pub(super) fn parse_with_max(text: &str, max_nesting: usize) -> Result<Filter, FilterParseError> {
    let mut parser = Parser {
        text,
        pos: 0,
        nesting: 0,
        max_nesting,
    };
    parser.skip_space();
    let filter = parser.parse_chain()?;
    parser.skip_space();
    if parser.pos != text.len() {
        return Err(FilterParseError::Syntax);
    }
    Ok(filter)
}

struct Parser<'a> {
    text: &'a str,
    pos: usize,
    nesting: usize,
    max_nesting: usize,
}

#[derive(Clone, Copy)]
enum PathKind {
    Operator,
    Selector,
}

impl Parser<'_> {
    fn rest(&self) -> &str {
        &self.text[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn eat(&mut self, prefix: &str) -> bool {
        if self.rest().starts_with(prefix) {
            self.pos += prefix.len();
            true
        } else {
            false
        }
    }

    fn skip_space(&mut self) -> bool {
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r'))
        {
            self.bump();
        }
        self.pos != start
    }

    fn parse_chain(&mut self) -> Result<Filter, FilterParseError> {
        let mut ops = Vec::new();
        loop {
            if self.peek() != Some(':') {
                return Err(FilterParseError::Syntax);
            }
            ops.push(self.parse_op()?);
            let spaced = self.skip_space();
            match self.peek() {
                Some(':') if spaced => return Err(FilterParseError::Syntax),
                Some(':') => {}
                Some(',') | Some(']') | None => break,
                Some(_) => return Err(FilterParseError::Syntax),
            }
        }
        Ok(if ops.len() == 1 {
            ops.pop().ok_or(FilterParseError::Syntax)?
        } else {
            Filter::Chain(ops)
        })
    }

    fn parse_op(&mut self) -> Result<Filter, FilterParseError> {
        if self.eat(":prefix=") {
            return Ok(Filter::Prefix(self.parse_path(PathKind::Operator)?.0));
        }
        if self.eat(":exclude[") {
            return self.parse_exclude();
        }
        if self.eat(":[") {
            return self.parse_compose();
        }
        if self.eat(":/") {
            return Ok(Filter::Subdir(self.parse_path(PathKind::Operator)?.0));
        }
        if self.eat(":nop") {
            return Ok(Filter::Nop);
        }
        if self.eat(":empty") {
            return Ok(Filter::Empty);
        }
        Err(FilterParseError::Syntax)
    }

    fn parse_compose(&mut self) -> Result<Filter, FilterParseError> {
        self.nesting += 1;
        if self.nesting > self.max_nesting {
            return Err(FilterParseError::NestingTooDeep);
        }
        self.skip_space();
        let mut members = Vec::new();
        loop {
            members.push(self.parse_chain()?);
            self.skip_space();
            if self.eat(",") {
                self.skip_space();
            } else if self.eat("]") {
                break;
            } else {
                return Err(FilterParseError::Syntax);
            }
        }
        self.nesting -= 1;
        Ok(Filter::Compose(members))
    }

    fn parse_exclude(&mut self) -> Result<Filter, FilterParseError> {
        self.skip_space();
        let mut selectors = Vec::new();
        loop {
            if !self.eat("::") {
                return Err(FilterParseError::ExcludeArgNotSelector);
            }
            let (path, tree) = self.parse_path(PathKind::Selector)?;
            selectors.push(if tree {
                Selector::Tree(path)
            } else {
                Selector::Entry(path)
            });
            self.skip_space();
            if self.eat(",") {
                self.skip_space();
            } else if self.eat("]") {
                break;
            } else {
                return Err(FilterParseError::Syntax);
            }
        }
        Ok(Filter::Exclude(selectors))
    }

    fn parse_path(&mut self, kind: PathKind) -> Result<(ViewPath, bool), FilterParseError> {
        let quoted = self.eat("\"");
        let mut raw = String::new();
        if quoted {
            loop {
                match self.bump() {
                    Some('"') => break,
                    Some('\\') if self.eat("\"") => raw.push('"'),
                    Some('\\') => return Err(FilterParseError::Backslash),
                    Some(ch) if ch.is_control() => return Err(FilterParseError::ControlChar),
                    Some(ch) => raw.push(ch),
                    None => return Err(FilterParseError::UnterminatedQuote),
                }
            }
        } else {
            loop {
                match self.peek() {
                    Some(':') | Some(',') | Some(']') | None => break,
                    Some(' ' | '\t' | '\n' | '\r') => break,
                    Some('"') => return Err(FilterParseError::PartialQuote),
                    Some('\\') => return Err(FilterParseError::Backslash),
                    Some(ch) if ch.is_control() => return Err(FilterParseError::ControlChar),
                    Some(ch) if ch == '/' || is_safe(ch) => {
                        raw.push(ch);
                        self.bump();
                    }
                    Some(_) => return Err(FilterParseError::NeedsQuoting),
                }
            }
            if self
                .peek()
                .is_some_and(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r'))
            {
                let next = self
                    .rest()
                    .trim_start_matches([' ', '\t', '\n', '\r'])
                    .chars()
                    .next();
                match next {
                    Some(':') => return Err(FilterParseError::Syntax),
                    Some(',') | Some(']') | None => {}
                    Some(_) => return Err(FilterParseError::NeedsQuoting),
                }
            }
        }

        let tree = match kind {
            PathKind::Selector if quoted => self.eat("/"),
            PathKind::Selector => {
                if raw.ends_with('/') {
                    raw.pop();
                    true
                } else {
                    false
                }
            }
            PathKind::Operator => {
                if raw.starts_with('/') {
                    raw.remove(0);
                }
                if raw.ends_with('/') {
                    raw.pop();
                }
                false
            }
        };

        if quoted {
            match self.peek() {
                Some('\\') => return Err(FilterParseError::Backslash),
                Some(ch) if ch == '/' || is_safe(ch) => {
                    return Err(FilterParseError::PartialQuote);
                }
                Some(ch) if !matches!(ch, ':' | ',' | ']' | ' ' | '\t' | '\n' | '\r') => {
                    return Err(FilterParseError::Syntax);
                }
                _ => {}
            }
        }

        let segments: Vec<String> = raw.split('/').map(str::to_owned).collect();
        for (index, segment) in segments.iter().enumerate() {
            if segment.is_empty() {
                if matches!(kind, PathKind::Selector) && index + 1 == segments.len() {
                    return Err(FilterParseError::EmptySelectorSegment);
                }
                return Err(FilterParseError::InvalidSegment);
            }
            if segment == "." || segment == ".." {
                return Err(FilterParseError::InvalidSegment);
            }
            if segment.chars().next().is_some_and(char::is_whitespace)
                || segment.chars().last().is_some_and(char::is_whitespace)
            {
                return Err(FilterParseError::SegmentEdgeWhitespace);
            }
        }
        Ok((ViewPath::new(segments), tree))
    }
}
