use crate::node::{
    BooleanBuilder, Comparison, ComparisonValue, LuceneClause, LuceneOccur, QueryNode,
};
use pest::iterators::Pair;
#[derive(Debug, Parser)]
#[grammar = "grammar.pest"]
pub struct EventPlatformQuery;

pub const DEFAULT_FIELD: &str = "_default_";
const EXISTS_FIELD: &str = "_exists_";
const MISSING_FIELD: &str = "_missing_";

/// The QueryVisitor is responsible for going through the output of our
/// parser and consuming the various tokens produced, digesting them and
/// converting them into QueryNodes.  As per the name, we're doing this
/// via a Visitor pattern and walking our way through the syntax tree.
pub struct QueryVisitor;

impl QueryVisitor {
    pub fn visit_queryroot(token: Pair<Rule>, default_field: &str) -> QueryNode {
        let contents = token.into_inner().next().unwrap();
        match contents.as_rule() {
            Rule::query => Self::visit_query(contents, default_field),
            // A queryroot will only ever contain a query
            _ => unreachable!(),
        }
    }

    fn visit_query(token: Pair<Rule>, default_field: &str) -> QueryNode {
        let contents = token.into_inner();
        let mut clauses: Vec<LuceneClause> = Vec::new();
        let mut modifier: Option<LuceneOccur> = None;
        for node in contents {
            let clause: Option<LuceneClause> = match node.as_rule() {
                Rule::multiterm => Some(Self::visit_multiterm(node, default_field)),
                Rule::conjunction => {
                    let inner = node.into_inner().next().unwrap();
                    match inner.as_rule() {
                        Rule::AND => {
                            // If our conjunction is AND and the previous clause was
                            // just a SHOULD, we make the previous clause a MUST and
                            // our new clause will also be a MUST
                            let mut lastitem = clauses.last_mut().unwrap();
                            if let LuceneOccur::Should = lastitem.occur {
                                lastitem.occur = LuceneOccur::Must;
                            };
                        }
                        Rule::OR => {
                            // If our conjunction is OR and the previous clause was
                            // a MUST, we make the previous clause a SHOULD and our
                            // new clause will also be a SHOULD
                            let mut lastitem = clauses.last_mut().unwrap();
                            if let LuceneOccur::Must = lastitem.occur {
                                lastitem.occur = LuceneOccur::Should;
                            };
                            modifier.get_or_insert(LuceneOccur::Should);
                        }
                        _ => unreachable!(),
                    };
                    None
                }
                Rule::modifiers => {
                    let inner = node.into_inner().next().unwrap();
                    match inner.as_rule() {
                        Rule::PLUS => (),
                        Rule::NOT => {
                            modifier = Some(LuceneOccur::MustNot);
                        }
                        _ => unreachable!(),
                    };
                    None
                }
                Rule::clause => {
                    let query_node = Self::visit_clause(node, default_field);
                    Some(LuceneClause {
                        occur: modifier.take().unwrap_or(LuceneOccur::Must),
                        node: query_node,
                    })
                }
                _ => unreachable!(),
            };
            // If we found a clause to add to our list, add it
            if let Some(c) = clause {
                clauses.push(c);
            }
        }
        if clauses.len() == 1 {
            let single = clauses.pop().unwrap();
            match single {
                LuceneClause {
                    occur: LuceneOccur::MustNot,
                    node: QueryNode::MatchAllDocs,
                } => return QueryNode::MatchNoDocs,
                // I hate Boxing!  Every allocation is a personal failing :(
                LuceneClause {
                    occur: LuceneOccur::MustNot,
                    node,
                } => {
                    return QueryNode::NegatedNode {
                        node: Box::new(node),
                    }
                }
                LuceneClause { occur: _, node } => return node,
            }
        } else {
            let mut and_builder = BooleanBuilder::and();
            let mut or_builder = BooleanBuilder::or();
            let (mut has_must, mut has_must_not, mut has_should) = (false, false, false);
            for c in clauses {
                let LuceneClause { node, occur } = c;
                match occur {
                    LuceneOccur::Must => {
                        and_builder.add_node(node);
                        has_must = true;
                    }
                    LuceneOccur::MustNot => {
                        and_builder.add_node(QueryNode::NegatedNode {
                            node: Box::new(node),
                        });
                        has_must_not = true;
                    }
                    LuceneOccur::Should => {
                        or_builder.add_node(node);
                        has_should = true;
                    }
                }
            }
            if has_must || !has_should {
                return and_builder.build();
            } else if !has_must_not {
                return or_builder.build();
            } else {
                and_builder.add_node(or_builder.build());
                return and_builder.build();
            }
        }
    }

    fn visit_multiterm(token: Pair<Rule>, default_field: &str) -> LuceneClause {
        let contents = token.into_inner();
        let mut terms: Vec<String> = Vec::new();
        for node in contents {
            match node.as_rule() {
                // Can probably get a bit more suave with string allocation here
                // but meh
                Rule::TERM => terms.push(Self::visit_term(node)),
                _ => unreachable!(),
            }
        }
        LuceneClause {
            occur: LuceneOccur::Must,
            node: QueryNode::AttributeTerm {
                attr: String::from(default_field),
                value: terms.join(" "),
            },
        }
    }

    fn visit_clause(clause: Pair<Rule>, default_field: &str) -> QueryNode {
        let mut field: Option<&str> = None;
        for item in clause.into_inner() {
            // As per the parser, a clause will only ever contain:
            // matchall, field, value, query
            match item.as_rule() {
                Rule::matchall => return QueryNode::MatchAllDocs,
                Rule::field => {
                    field = Some(Self::visit_field(item));
                }
                Rule::value => {
                    // As per the parser, value can only ever be one of:
                    // STAR, PHRASE, TERM, TERM_PREFIX, TERM_GLOB, range, comparison
                    let value_contents = item.into_inner().next().unwrap();
                    match (
                        (field.clone().unwrap_or(default_field)),
                        value_contents.as_rule(),
                    ) {
                        (EXISTS_FIELD, Rule::TERM) => {
                            return QueryNode::AttributeExists {
                                attr: Self::visit_term(value_contents),
                            }
                        }
                        (EXISTS_FIELD, Rule::PHRASE) => {
                            return QueryNode::AttributeExists {
                                attr: Self::visit_phrase(value_contents),
                            }
                        }
                        (MISSING_FIELD, Rule::TERM) => {
                            return QueryNode::AttributeMissing {
                                attr: Self::visit_term(value_contents),
                            }
                        }
                        (MISSING_FIELD, Rule::PHRASE) => {
                            return QueryNode::AttributeMissing {
                                attr: Self::visit_phrase(value_contents),
                            }
                        }
                        (DEFAULT_FIELD, Rule::STAR) => return QueryNode::MatchAllDocs,
                        (f @ _, Rule::STAR) => {
                            return QueryNode::AttributeWildcard {
                                attr: unescape(f),
                                wildcard: String::from("*"),
                            }
                        }
                        (f @ _, Rule::TERM) => {
                            return QueryNode::AttributeTerm {
                                attr: unescape(f),
                                value: Self::visit_term(value_contents),
                            }
                        }
                        (f @ _, Rule::PHRASE) => {
                            return QueryNode::QuotedAttribute {
                                attr: unescape(f),
                                phrase: Self::visit_phrase(value_contents),
                            }
                        }
                        (f @ _, Rule::TERM_PREFIX) => {
                            return QueryNode::AttributePrefix {
                                attr: unescape(f),
                                prefix: Self::visit_prefix(value_contents),
                            }
                        }
                        (f @ _, Rule::TERM_GLOB) => {
                            return QueryNode::AttributeWildcard {
                                attr: unescape(f),
                                wildcard: Self::visit_wildcard(value_contents),
                            }
                        }
                        (f @ _, Rule::range) => {
                            let mut range_values = value_contents.into_inner();
                            let lower = Self::visit_range_value(range_values.next().unwrap());
                            let lower_inclusive = true;
                            let upper = Self::visit_range_value(range_values.next().unwrap());
                            let upper_inclusive = true;
                            return QueryNode::AttributeRange {
                                attr: unescape(f),
                                lower,
                                lower_inclusive,
                                upper,
                                upper_inclusive,
                            };
                        }
                        (f @ _, Rule::comparison) => {
                            let mut compiter = value_contents.into_inner();
                            let comparator = Self::visit_operator(
                                compiter.next().unwrap().into_inner().next().unwrap(),
                            );
                            let comparison_value = compiter.next().unwrap();
                            let value = match comparison_value.as_rule() {
                                Rule::TERM => {
                                    ComparisonValue::String(Self::visit_term(comparison_value))
                                }
                                Rule::PHRASE => {
                                    ComparisonValue::String(Self::visit_phrase(comparison_value))
                                }
                                Rule::NUMERIC_TERM => {
                                    ComparisonValue::Numeric(Self::visit_num_term(comparison_value))
                                }
                                _ => unreachable!(),
                            };
                            return QueryNode::AttributeComparison {
                                attr: unescape(f),
                                comparator,
                                value,
                            };
                        }
                        // We've covered all the cases, so this should never happen
                        _ => unreachable!(),
                    }
                }
                Rule::query => {
                    return Self::visit_query(item, field.as_deref().unwrap_or(default_field))
                }
                // We've covered all the cases, so this should never happen
                _ => unreachable!(),
            }
        }
        QueryNode::MatchAllDocs
    }

    fn visit_operator(token: Pair<Rule>) -> Comparison {
        match token.as_rule() {
            Rule::GT => Comparison::GT,
            Rule::GT_EQ => Comparison::GTE,
            Rule::LT => Comparison::LT,
            Rule::LT_EQ => Comparison::LTE,
            _ => unreachable!(),
        }
    }

    fn visit_range_value(token: Pair<Rule>) -> ComparisonValue {
        match token.as_rule() {
            Rule::RANGE_VALUE => {
                if token.as_str() == "*" {
                    ComparisonValue::Unbounded
                } else {
                    ComparisonValue::String(unescape(token.as_str()))
                }
            }
            _ => unreachable!(),
        }
    }

    fn visit_term(token: Pair<Rule>) -> String {
        unescape(token.as_str())
    }

    fn visit_prefix(token: Pair<Rule>) -> String {
        let prefix_string = token.as_str();
        unescape(&prefix_string[..prefix_string.len() - 1])
    }

    fn visit_wildcard(token: Pair<Rule>) -> String {
        unescape(token.as_str())
    }

    fn visit_phrase(token: Pair<Rule>) -> String {
        let quoted_string = token.as_str();
        unescape(&quoted_string[1..quoted_string.len() - 1])
    }

    fn visit_num_term(token: Pair<Rule>) -> f64 {
        // TODO:  Error handling here should be better
        unescape(token.as_str()).parse::<f64>().unwrap_or_default()
    }

    fn visit_field(token: Pair<Rule>) -> &str {
        let inner = token.into_inner().next().unwrap();
        if let Rule::TERM = inner.as_rule() {
            return inner.as_str();
        }
        return "BROKEN";
    }
}

/// Remove escaped characters from a string, returning a newly allocated
/// unescaped string.  At this point we do NOT distinguish between chars
/// that REQUIRE escaping and those that don't, so we'll preserve anything
/// with a '\' before it
fn unescape(input: &str) -> String {
    // Unescaping will only ever make a string shorter so let's go...
    let mut output = String::with_capacity(input.len());
    let mut escape_sequence = false;
    for c in input.chars() {
        if escape_sequence {
            output.push(c);
            escape_sequence = false;
        } else if c == '\\' {
            escape_sequence = true;
        } else {
            output.push(c)
        }
    }
    // TODO:  Check for unterminated escape sequence and signal a problem
    output
}