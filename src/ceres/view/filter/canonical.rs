use std::collections::VecDeque;

use super::{Filter, Selector, ViewPath};

pub fn canonicalize(filter: Filter) -> Filter {
    match filter {
        Filter::Subdir(path) => Filter::Subdir(path),
        Filter::Prefix(path) => Filter::Prefix(path),
        Filter::Exclude(mut selectors) => {
            selectors.sort_by_cached_key(print_selector);
            selectors.dedup();
            if selectors.is_empty() {
                Filter::Nop
            } else {
                Filter::Exclude(selectors)
            }
        }
        Filter::Compose(children) => {
            let mut members = Vec::new();
            for child in children {
                match canonicalize(child) {
                    Filter::Empty => {}
                    Filter::Compose(nested) => members.extend(nested),
                    other => members.push(other),
                }
            }
            members.sort_by_cached_key(print);
            match members.len() {
                0 => Filter::Empty,
                1 => members.pop().unwrap_or(Filter::Empty),
                _ => Filter::Compose(members),
            }
        }
        Filter::Chain(children) => canonicalize_chain(children),
        Filter::Nop => Filter::Nop,
        Filter::Empty => Filter::Empty,
    }
}

fn canonicalize_chain(children: Vec<Filter>) -> Filter {
    let mut ops = Vec::new();
    for child in children {
        match canonicalize(child) {
            Filter::Empty => return Filter::Empty,
            Filter::Nop => {}
            Filter::Chain(nested) => {
                for op in nested {
                    if !push_op(&mut ops, ChainOp::from(op)) {
                        return Filter::Empty;
                    }
                }
            }
            other => {
                if !push_op(&mut ops, ChainOp::from(other)) {
                    return Filter::Empty;
                }
            }
        }
    }
    match ops.len() {
        0 => Filter::Nop,
        1 => ops.pop().map(ChainOp::into_filter).unwrap_or(Filter::Nop),
        _ => Filter::Chain(ops.into_iter().map(ChainOp::into_filter).collect()),
    }
}

enum ChainOp {
    Subdir(VecDeque<String>),
    Prefix(VecDeque<String>),
    Other(Filter),
}

impl From<Filter> for ChainOp {
    fn from(filter: Filter) -> Self {
        match filter {
            Filter::Subdir(path) => Self::Subdir(path.segments.into()),
            Filter::Prefix(path) => Self::Prefix(path.segments.into()),
            other => Self::Other(other),
        }
    }
}

impl ChainOp {
    fn into_filter(self) -> Filter {
        match self {
            Self::Subdir(segments) => Filter::Subdir(ViewPath::new(segments.into())),
            Self::Prefix(segments) => Filter::Prefix(ViewPath::new(segments.into())),
            Self::Other(filter) => filter,
        }
    }
}

fn push_op(ops: &mut Vec<ChainOp>, mut current: ChainOp) -> bool {
    while let Some(previous) = ops.pop() {
        current = match (previous, current) {
            (ChainOp::Subdir(mut a), ChainOp::Subdir(mut b)) => {
                a.append(&mut b);
                ChainOp::Subdir(a)
            }
            (ChainOp::Prefix(mut a), ChainOp::Prefix(mut b)) => {
                while let Some(segment) = b.pop_back() {
                    a.push_front(segment);
                }
                ChainOp::Prefix(a)
            }
            (ChainOp::Prefix(mut p), ChainOp::Subdir(mut q)) => {
                let common = p.iter().zip(q.iter()).take_while(|(a, b)| a == b).count();
                if common < p.len().min(q.len()) {
                    return false;
                }
                for _ in 0..common {
                    p.pop_front();
                    q.pop_front();
                }
                if p.is_empty() && q.is_empty() {
                    return true;
                }
                if p.is_empty() {
                    ChainOp::Subdir(q)
                } else {
                    ChainOp::Prefix(p)
                }
            }
            (a, b) => {
                ops.push(a);
                ops.push(b);
                return true;
            }
        };
    }
    ops.push(current);
    true
}

pub fn print(filter: &Filter) -> String {
    match filter {
        Filter::Subdir(path) => format!(":/{}", print_path(path)),
        Filter::Prefix(path) => format!(":prefix={}", print_path(path)),
        Filter::Exclude(selectors) => {
            format!(
                ":exclude[{}]",
                selectors
                    .iter()
                    .map(print_selector)
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Filter::Compose(members) => {
            format!(
                ":[{}]",
                members.iter().map(print).collect::<Vec<_>>().join(",")
            )
        }
        Filter::Chain(ops) => ops.iter().map(print).collect(),
        Filter::Nop => ":nop".to_owned(),
        Filter::Empty => ":empty".to_owned(),
    }
}

pub(super) fn print_selector(selector: &Selector) -> String {
    match selector {
        Selector::Entry(path) => format!("::{}", print_path(path)),
        Selector::Tree(path) => format!("::{}/", print_path(path)),
    }
}

pub(super) fn print_path(path: &ViewPath) -> String {
    let text = path.segments.join("/");
    if text.chars().all(|ch| ch == '/' || is_safe(ch)) {
        text
    } else {
        format!("\"{}\"", text.replace('"', "\\\""))
    }
}

pub(super) fn is_safe(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || matches!(ch, '.' | '_' | '-')
        || (!ch.is_ascii() && !ch.is_whitespace() && !ch.is_control())
}
