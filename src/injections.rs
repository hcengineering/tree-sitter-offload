use std::{
    collections::HashMap,
    ops::{Deref, Range},
};

use streaming_iterator::StreamingIterator;
use tree_sitter as ts;

use crate::{
    language_registry::UnknownLanguage,
    predicates::AdditionalPredicates,
    query::{CaptureOffset, RecodingUtf16TextProvider},
};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum InjectionLanguage {
    #[default]
    NotSpecified,
    Static(UnknownLanguage),
}

pub struct InjectionMatch {
    pub id: usize,
    pub language: UnknownLanguage,
    pub enclosing_byte_range: Range<usize>,
    pub included_ranges: Vec<tree_sitter::Range>,
    pub combined: bool,
    pub include_children: bool,
}

#[derive(Default)]
struct InjectionInfo {
    language: InjectionLanguage,
    offsets: HashMap<u32, CaptureOffset>,
    combined: bool,
    include_children: bool,
}

pub struct InjectionQuery {
    query: ts::Query,
    predicates: AdditionalPredicates,
    injection_content_capture_id: u32,
    injection_language_capture_id: Option<u32>,
    injection_mimetype_capture_id: Option<u32>,
    injections: Vec<InjectionInfo>,
}

#[derive(thiserror::Error, Debug)]
pub enum InjectionQueryError {
    #[error("required captures not found")]
    NoRequiredCaptures,
    #[error("duplicate captures found")]
    DuplicateCapture,
    #[error("Invalid property \"{1}\" for pattern {0}")]
    InvalidPatternProperty(usize, Box<str>),
    #[error("Conflicting languages \"{1:?}\" and \"{2:?}\" for pattern {0}")]
    LanguageConflict(usize, InjectionLanguage, InjectionLanguage),
    #[error("Invalid predicatte \"{1}\" for pattern {0}")]
    InvalidPredicate(usize, Box<str>),
}

impl InjectionQuery {
    pub fn new(
        query: ts::Query,
        predicates: AdditionalPredicates,
    ) -> Result<InjectionQuery, InjectionQueryError> {
        let mut injection_content_capture_id: Option<u32> = None;
        let mut injection_language_capture_id: Option<u32> = None;
        let mut injection_mimetype_capture_id: Option<u32> = None;
        for (idx, capture_name) in query.capture_names().iter().enumerate() {
            match *capture_name {
                "injection.content" => {
                    let old_capture_id = injection_content_capture_id.replace(idx as u32);
                    if old_capture_id.is_some() {
                        return Err(InjectionQueryError::DuplicateCapture);
                    }
                }
                "injection.language" => {
                    let old_capture_id = injection_language_capture_id.replace(idx as u32);
                    if old_capture_id.is_some() {
                        return Err(InjectionQueryError::DuplicateCapture);
                    }
                }
                "injection.mimetype" => {
                    let old_capture_id = injection_mimetype_capture_id.replace(idx as u32);
                    if old_capture_id.is_some() {
                        return Err(InjectionQueryError::DuplicateCapture);
                    }
                }
                _ => (),
            }
        }
        let injection_content_capture_id =
            injection_content_capture_id.ok_or(InjectionQueryError::NoRequiredCaptures)?;
        let injections: Vec<InjectionInfo> = Vec::with_capacity(query.pattern_count());
        let mut result = InjectionQuery {
            query,
            predicates,
            injection_content_capture_id,
            injection_language_capture_id,
            injection_mimetype_capture_id,
            injections,
        };
        for pattern_idx in 0..result.query.pattern_count() {
            let mut injection_info = InjectionInfo::default();
            for setting in result.query.property_settings(pattern_idx) {
                match setting.key.deref() {
                    "injection.language" => {
                        let ts::QueryProperty {
                            key: _,
                            capture_id: None,
                            value: Some(ref language_name),
                        } = setting
                        else {
                            return Err(InjectionQueryError::InvalidPatternProperty(
                                pattern_idx,
                                setting.key.clone(),
                            ));
                        };
                        if injection_info.language != InjectionLanguage::NotSpecified {
                            return Err(InjectionQueryError::LanguageConflict(
                                pattern_idx,
                                injection_info.language.clone(),
                                InjectionLanguage::Static(UnknownLanguage::LanguageName(
                                    language_name.clone(),
                                )),
                            ));
                        }
                        injection_info.language = InjectionLanguage::Static(
                            UnknownLanguage::LanguageName(language_name.clone()),
                        );
                    }
                    "injection.combined" => {
                        let ts::QueryProperty {
                            key: _,
                            capture_id: None,
                            value: None,
                        } = setting
                        else {
                            return Err(InjectionQueryError::InvalidPatternProperty(
                                pattern_idx,
                                setting.key.clone(),
                            ));
                        };
                        injection_info.combined = true;
                    }
                    "injection.include-children" => {
                        let ts::QueryProperty {
                            key: _,
                            capture_id: None,
                            value: None,
                        } = setting
                        else {
                            return Err(InjectionQueryError::InvalidPatternProperty(
                                pattern_idx,
                                setting.key.clone(),
                            ));
                        };
                        injection_info.include_children = true;
                    }
                    _ => (),
                }
            }
            for predicate in result.query.general_predicates(pattern_idx) {
                if predicate.operator.deref() == "offset!" {
                    match predicate.args.deref() {
                        [ts::QueryPredicateArg::Capture(capture_id), ts::QueryPredicateArg::String(arg1), ts::QueryPredicateArg::String(arg2)] =>
                        {
                            let (Ok(arg1), Ok(arg2)) =
                                (str::parse::<i32>(arg1), str::parse::<i32>(arg2))
                            else {
                                return Err(InjectionQueryError::InvalidPredicate(
                                    pattern_idx,
                                    predicate.operator.clone(),
                                ));
                            };
                            injection_info
                                .offsets
                                .insert(*capture_id, CaptureOffset::new(arg1 * 2, arg2 * 2));
                        }
                        _ => {
                            return Err(InjectionQueryError::InvalidPredicate(
                                pattern_idx,
                                predicate.operator.clone(),
                            ));
                        }
                    }
                }
            }
            result.injections.push(injection_info);
        }
        Ok(result)
    }

    pub fn collect_injections(
        &self,
        node: tree_sitter::Node,
        text: &[u16],
        changed_byte_ranges: &[std::ops::Range<usize>],
    ) -> Vec<InjectionMatch> {
        let mut query_cursor = ts::QueryCursor::new();
        let text_provider = RecodingUtf16TextProvider::new(text);
        let mut injections: Vec<InjectionMatch> = Vec::new();
        let mut injection_ranges: HashMap<Range<usize>, usize> = HashMap::new();
        for change_byte_range in changed_byte_ranges {
            query_cursor.set_byte_range(
                change_byte_range.start.saturating_sub(2)..(change_byte_range.end + 2),
            );
            let mut matches = query_cursor.matches(&self.query, node, &text_provider);
            while let Some(query_match) = matches.next() {
                if !self
                    .predicates
                    .satisfies_predicates(&mut &text_provider, query_match)
                {
                    continue;
                }
                let info = &self.injections[query_match.pattern_index];
                let mut query_ranges: Vec<ts::Range> = Vec::new();
                let mut query_language: Option<UnknownLanguage> = None;
                for capture in query_match.captures.iter() {
                    let range = if let Some(offset) = info.offsets.get(&capture.index) {
                        offset.apply_to_range(&capture.node.range())
                    } else {
                        capture.node.range()
                    };
                    if self.injection_content_capture_id == capture.index {
                        query_ranges.push(range);
                    }
                    if self.injection_language_capture_id == Some(capture.index) {
                        let language = String::from_utf16_lossy(
                            &text[(range.start_byte / 2)..(range.end_byte / 2)],
                        );
                        query_language = Some(UnknownLanguage::LanguageName(language.into()));
                    }
                    if self.injection_mimetype_capture_id == Some(capture.index) {
                        let mimetype = String::from_utf16_lossy(
                            &text[(range.start_byte / 2)..(range.end_byte / 2)],
                        );
                        query_language = Some(UnknownLanguage::LanguageMimetype(mimetype.into()));
                    }
                }
                if query_ranges.is_empty() {
                    continue;
                }
                let language = match &info.language {
                    InjectionLanguage::NotSpecified => {
                        let Some(language) = query_language else {
                            continue;
                        };
                        language
                    }
                    InjectionLanguage::Static(language) => language.clone(),
                };
                let range_start = query_ranges.first().expect("ranges are not empty");
                let range_end = query_ranges.last().expect("ranges are not empty");
                let enclosing_byte_range = range_start.start_byte..range_end.end_byte;
                if let Some(injection_idx) = injection_ranges.get(&enclosing_byte_range) {
                    injections[*injection_idx] = InjectionMatch {
                        id: query_match.pattern_index,
                        language,
                        enclosing_byte_range,
                        included_ranges: query_ranges,
                        combined: info.combined,
                        include_children: info.include_children,
                    };
                } else {
                    injection_ranges.insert(enclosing_byte_range.clone(), injections.len());
                    injections.push(InjectionMatch {
                        id: query_match.pattern_index,
                        language,
                        enclosing_byte_range,
                        included_ranges: query_ranges,
                        combined: info.combined,
                        include_children: info.include_children,
                    });
                }
            }
        }
        injections
    }
}
