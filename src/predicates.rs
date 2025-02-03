use std::{collections::HashMap, marker::PhantomData, ops::Deref};

use tree_sitter::{
    Node, Query, QueryError, QueryErrorKind, QueryMatch, QueryPredicate, QueryPredicateArg,
    TextProvider,
};

const fn predicate_error(row: usize, message: String) -> QueryError {
    QueryError {
        row,
        column: 0,
        offset: 0,
        message,
        kind: QueryErrorKind::Predicate,
    }
}

pub trait TextProviderPredicate {
    fn text(&mut self, node: Node) -> &[u8];
}

struct TextProviderPredicateImpl<'a, T: TextProvider<I>, I: AsRef<[u8]>> {
    text_provider: &'a mut T,
    buffer: Vec<u8>,
    _phantom: PhantomData<I>,
}

impl<T: TextProvider<I>, I: AsRef<[u8]>> TextProviderPredicate
    for TextProviderPredicateImpl<'_, T, I>
{
    fn text(&mut self, node: Node) -> &[u8] {
        let chunks = self.text_provider.text(node);
        self.buffer.clear();
        for chunk in chunks {
            self.buffer.extend_from_slice(chunk.as_ref());
        }
        &self.buffer
    }
}

pub trait Predicate {
    fn check_predicate(
        &self,
        mat: &QueryMatch<'_, '_>,
        text: &mut dyn TextProviderPredicate,
    ) -> bool;
}

pub trait PredicateParser {
    fn can_parse_predicate(&self, name: &str) -> bool;
    fn parse_predicate(
        &self,
        query: &Query,
        row: usize,
        predicate: &QueryPredicate,
    ) -> Result<Box<dyn Predicate + Send + Sync>, QueryError>;
}

impl PredicateParser for HashMap<&'static str, Box<dyn PredicateParser>> {
    fn can_parse_predicate(&self, name: &str) -> bool {
        self.get(&name).is_some_and(|p| p.can_parse_predicate(name))
    }

    fn parse_predicate(
        &self,
        query: &Query,
        row: usize,
        predicate: &QueryPredicate,
    ) -> Result<Box<dyn Predicate + Send + Sync>, QueryError> {
        let parser = self.get(predicate.operator.deref()).ok_or_else(|| {
            predicate_error(
                row,
                format!("Unknown predicate operator {}", predicate.operator),
            )
        })?;
        parser.parse_predicate(query, row, predicate)
    }
}

#[derive(Clone, Copy)]
pub struct ContainsPredicateParser;

struct ContainsPredicate {
    capture_id: u32,
    pattern: Box<str>,
    is_positive: bool,
    match_all: bool,
}

impl PredicateParser for ContainsPredicateParser {
    fn can_parse_predicate(&self, name: &str) -> bool {
        [
            "contains?",
            "not-contains?",
            "any-contains?",
            "any-not-contains?",
        ]
        .contains(&name)
    }
    fn parse_predicate(
        &self,
        query: &Query,
        row: usize,
        predicate: &QueryPredicate,
    ) -> Result<Box<dyn Predicate + Send + Sync>, QueryError> {
        let (is_positive, match_all) = match predicate.operator.deref() {
            "contains?" => (true, true),
            "not-contains?" => (false, true),
            "any-contains?" => (true, false),
            "any-not-contains?" => (false, false),
            _ => {
                return Err(predicate_error(
                    row,
                    format!("Invalid operator {}", predicate.operator),
                ));
            }
        };
        if predicate.args.len() != 2 {
            return Err(predicate_error(
                row,
                format!(
                    "Wrong number of arguments to #{} predicate. Expected 2, got {}",
                    predicate.operator,
                    predicate.args.len()
                ),
            ));
        }
        let capture_id = match &predicate.args[0] {
            QueryPredicateArg::Capture(capture_id) => *capture_id,
            QueryPredicateArg::String(literal) => {
                return Err(predicate_error(
                    row,
                    format!(
                        "First argument to #{} predicate must be a capture name. Got literal \"{}\".",
                        predicate.operator, literal
                    ),
                ));
            }
        };
        let pattern = match &predicate.args[1] {
            QueryPredicateArg::Capture(capture_id) => {
                return Err(predicate_error(
                    row,
                    format!(
                        "Second argument to #{} predicate must be a literal. Got capture @{}.",
                        predicate.operator,
                        query.capture_names()[*capture_id as usize]
                    ),
                ));
            }
            QueryPredicateArg::String(literal) => literal.clone(),
        };

        Ok(Box::new(ContainsPredicate {
            capture_id,
            pattern,
            is_positive,
            match_all,
        }))
    }
}

impl Predicate for ContainsPredicate {
    fn check_predicate(
        &self,
        mat: &QueryMatch<'_, '_>,
        texts: &mut dyn TextProviderPredicate,
    ) -> bool {
        for node in mat.nodes_for_capture_index(self.capture_id) {
            let text = texts.text(node);
            let text = String::from_utf8_lossy(text);
            let does_match = text.contains(self.pattern.deref());
            if does_match != self.is_positive && self.match_all {
                return false;
            }
            if does_match == self.is_positive && !self.match_all {
                return true;
            }
        }
        self.match_all
    }
}

type AnyPredicate = Box<dyn Predicate + Send + Sync>;

pub struct AdditionalPredicates {
    predicates: Box<[Box<[AnyPredicate]>]>,
}

impl AdditionalPredicates {
    pub fn parse(
        query: &Query,
        source: &str,
        parser: &impl PredicateParser,
    ) -> Result<Self, QueryError> {
        let mut additional_predicates = Vec::with_capacity(query.pattern_count());
        for pattern_idx in 0..query.pattern_count() {
            let pattern_start = query.start_byte_for_pattern(pattern_idx);
            let row = source
                .char_indices()
                .take_while(|(i, _)| *i < pattern_start)
                .filter(|(_, c)| *c == '\n')
                .count();
            let general_predicates = query.general_predicates(pattern_idx);
            let mut parsed_predicates = Vec::with_capacity(general_predicates.len());
            for predicate in query.general_predicates(pattern_idx) {
                if !parser.can_parse_predicate(predicate.operator.deref()) {
                    continue;
                }
                parsed_predicates.push(parser.parse_predicate(query, row, predicate)?);
            }
            additional_predicates.push(parsed_predicates.into());
        }
        Ok(Self {
            predicates: additional_predicates.into(),
        })
    }

    pub fn satisfies_predicates<I: AsRef<[u8]>>(
        &self,
        text_provider: &mut impl TextProvider<I>,
        query_match: &QueryMatch,
    ) -> bool {
        let Some(predicates) = self.predicates.get(query_match.pattern_index) else {
            return true;
        };
        let mut predicate_text_provider = TextProviderPredicateImpl {
            text_provider,
            buffer: Vec::with_capacity(64),
            _phantom: PhantomData,
        };
        for predicate in predicates {
            if !predicate.check_predicate(query_match, &mut predicate_text_provider) {
                return false;
            }
        }
        true
    }
}

thread_local! {
    pub(crate) static PREDICATE_PARSER: HashMap<&'static str, Box<dyn PredicateParser>> = HashMap::from([
        ("contains?", Box::new(ContainsPredicateParser) as Box<dyn PredicateParser>),
        ("not-contains?", Box::new(ContainsPredicateParser) as Box<dyn PredicateParser>),
        ("any-contains?", Box::new(ContainsPredicateParser) as Box<dyn PredicateParser>),
        ("any-not-contains?", Box::new(ContainsPredicateParser) as Box<dyn PredicateParser>),
    ]);
}
