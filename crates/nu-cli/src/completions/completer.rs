use crate::completions::{
    AttributableCompletion, AttributeCompletion, CellPathCompletion, CommandCompletion, Completer,
    CompletionOptions, CustomCompletion, DirectoryCompletion, DotNuCompletion, FileCompletion,
    FlagCompletion, OperatorCompletion, VariableCompletion,
};
use log::debug;
use nu_color_config::{color_record_to_nustyle, lookup_ansi_color_style};
use nu_engine::eval_block;
use nu_parser::{flatten_expression, parse, FlatShape};
use nu_protocol::{
    ast::{Expr, Expression, FindMapResult, Traverse},
    debugger::WithoutDebug,
    engine::{Closure, EngineState, Stack, StateWorkingSet},
    PipelineData, Span, Value,
};
use reedline::{Completer as ReedlineCompleter, Suggestion};
use std::{str, sync::Arc};

use super::base::{SemanticSuggestion, SuggestionKind};

/// Used as the function `f` in find_map Traverse
///
/// returns the inner-most pipeline_element of interest
/// i.e. the one that contains given position and needs completion
fn find_pipeline_element_by_position<'a>(
    expr: &'a Expression,
    working_set: &'a StateWorkingSet,
    pos: usize,
) -> FindMapResult<&'a Expression> {
    // skip the entire expression if the position is not in it
    if !expr.span.contains(pos) {
        return FindMapResult::Stop;
    }
    let closure = |expr: &'a Expression| find_pipeline_element_by_position(expr, working_set, pos);
    match &expr.expr {
        Expr::Call(call) => call
            .arguments
            .iter()
            .find_map(|arg| arg.expr().and_then(|e| e.find_map(working_set, &closure)))
            // if no inner call/external_call found, then this is the inner-most one
            .or(Some(expr))
            .map(FindMapResult::Found)
            .unwrap_or_default(),
        // TODO: clear separation of internal/external completion logic
        Expr::ExternalCall(head, arguments) => arguments
            .iter()
            .find_map(|arg| arg.expr().find_map(working_set, &closure))
            .or(head.as_ref().find_map(working_set, &closure))
            .or(Some(expr))
            .map(FindMapResult::Found)
            .unwrap_or_default(),
        // complete the operator
        Expr::BinaryOp(lhs, _, rhs) => lhs
            .find_map(working_set, &closure)
            .or(rhs.find_map(working_set, &closure))
            .or(Some(expr))
            .map(FindMapResult::Found)
            .unwrap_or_default(),
        Expr::FullCellPath(fcp) => fcp
            .head
            .find_map(working_set, &closure)
            .or(Some(expr))
            .map(FindMapResult::Found)
            .unwrap_or_default(),
        Expr::Var(_) => FindMapResult::Found(expr),
        Expr::AttributeBlock(ab) => ab
            .attributes
            .iter()
            .map(|attr| &attr.expr)
            .chain(Some(ab.item.as_ref()))
            .find_map(|expr| expr.find_map(working_set, &closure))
            .or(Some(expr))
            .map(FindMapResult::Found)
            .unwrap_or_default(),
        _ => FindMapResult::Continue,
    }
}

/// Before completion, an additional character `a` is added to the source as a placeholder for correct parsing results.
/// This function helps to strip it
fn strip_placeholder<'a>(working_set: &'a StateWorkingSet, span: &Span) -> (Span, &'a [u8]) {
    let new_end = std::cmp::max(span.end - 1, span.start);
    let new_span = Span::new(span.start, new_end);
    let prefix = working_set.get_span_contents(new_span);
    (new_span, prefix)
}

#[derive(Clone)]
pub struct NuCompleter {
    engine_state: Arc<EngineState>,
    stack: Stack,
}

impl NuCompleter {
    pub fn new(engine_state: Arc<EngineState>, stack: Arc<Stack>) -> Self {
        Self {
            engine_state,
            stack: Stack::with_parent(stack).reset_out_dest().collect_value(),
        }
    }

    pub fn fetch_completions_at(&mut self, line: &str, pos: usize) -> Vec<SemanticSuggestion> {
        self.completion_helper(line, pos)
    }

    fn variable_names_completion_helper(
        &self,
        working_set: &StateWorkingSet,
        span: Span,
        offset: usize,
    ) -> Vec<SemanticSuggestion> {
        let (new_span, prefix) = strip_placeholder(working_set, &span);
        if !prefix.starts_with(b"$") {
            return vec![];
        }
        let mut variable_names_completer = VariableCompletion {};
        self.process_completion(
            &mut variable_names_completer,
            working_set,
            prefix,
            new_span,
            offset,
            // pos is not required
            0,
        )
    }

    // Process the completion for a given completer
    fn process_completion<T: Completer>(
        &self,
        completer: &mut T,
        working_set: &StateWorkingSet,
        prefix: &[u8],
        new_span: Span,
        offset: usize,
        pos: usize,
    ) -> Vec<SemanticSuggestion> {
        let config = self.engine_state.get_config();

        let options = CompletionOptions {
            case_sensitive: config.completions.case_sensitive,
            match_algorithm: config.completions.algorithm.into(),
            sort: config.completions.sort,
            ..Default::default()
        };

        debug!(
            "process_completion: prefix: {}, new_span: {new_span:?}, offset: {offset}, pos: {pos}",
            String::from_utf8_lossy(prefix)
        );

        completer.fetch(
            working_set,
            &self.stack,
            prefix,
            new_span,
            offset,
            pos,
            &options,
        )
    }

    fn external_completion(
        &self,
        closure: &Closure,
        spans: &[String],
        offset: usize,
        span: Span,
    ) -> Option<Vec<SemanticSuggestion>> {
        let block = self.engine_state.get_block(closure.block_id);
        let mut callee_stack = self
            .stack
            .captures_to_stack_preserve_out_dest(closure.captures.clone());

        // Line
        if let Some(pos_arg) = block.signature.required_positional.first() {
            if let Some(var_id) = pos_arg.var_id {
                callee_stack.add_var(
                    var_id,
                    Value::list(
                        spans
                            .iter()
                            .map(|it| Value::string(it, Span::unknown()))
                            .collect(),
                        Span::unknown(),
                    ),
                );
            }
        }

        let result = eval_block::<WithoutDebug>(
            &self.engine_state,
            &mut callee_stack,
            block,
            PipelineData::empty(),
        );

        match result.and_then(|data| data.into_value(span)) {
            Ok(Value::List { vals, .. }) => {
                let result =
                    map_value_completions(vals.iter(), Span::new(span.start, span.end), offset);
                Some(result)
            }
            Ok(Value::Nothing { .. }) => None,
            Ok(value) => {
                log::error!(
                    "External completer returned invalid value of type {}",
                    value.get_type().to_string()
                );
                Some(vec![])
            }
            Err(err) => {
                log::error!("failed to eval completer block: {err}");
                Some(vec![])
            }
        }
    }

    fn completion_helper(&mut self, line: &str, pos: usize) -> Vec<SemanticSuggestion> {
        let mut working_set = StateWorkingSet::new(&self.engine_state);
        let offset = working_set.next_span_start();
        // TODO: Callers should be trimming the line themselves
        let line = if line.len() > pos { &line[..pos] } else { line };
        // Adjust offset so that the spans of the suggestions will start at the right
        // place even with `only_buffer_difference: true`
        let fake_offset = offset + line.len() - pos;
        let pos = offset + line.len();
        let initial_line = line.to_string();
        let mut line = line.to_string();
        line.push('a');

        let config = self.engine_state.get_config();

        let block = parse(&mut working_set, Some("completer"), line.as_bytes(), false);
        let Some(element_expression) = block.find_map(&working_set, &|expr: &Expression| {
            find_pipeline_element_by_position(expr, &working_set, pos)
        }) else {
            return vec![];
        };

        match &element_expression.expr {
            Expr::Var(_) => {
                return self.variable_names_completion_helper(
                    &working_set,
                    element_expression.span,
                    fake_offset,
                );
            }
            Expr::FullCellPath(full_cell_path) => {
                // e.g. `$e<tab>` parsed as FullCellPath
                if full_cell_path.tail.is_empty() {
                    return self.variable_names_completion_helper(
                        &working_set,
                        element_expression.span,
                        fake_offset,
                    );
                } else {
                    let mut cell_path_completer = CellPathCompletion { full_cell_path };
                    return self.process_completion(
                        &mut cell_path_completer,
                        &working_set,
                        &[],
                        element_expression.span,
                        fake_offset,
                        pos,
                    );
                }
            }
            _ => (),
        }

        let flattened = flatten_expression(&working_set, element_expression);
        let mut spans: Vec<String> = vec![];

        for (flat_idx, (span, shape)) in flattened.iter().enumerate() {
            let is_passthrough_command = spans
                .first()
                .filter(|content| content.as_str() == "sudo" || content.as_str() == "doas")
                .is_some();

            // Read the current span to string
            let current_span = working_set.get_span_contents(*span);
            let current_span_str = String::from_utf8_lossy(current_span);
            let is_last_span = span.contains(pos);

            // Skip the last 'a' as span item
            if is_last_span {
                let offset = pos - span.start;
                if offset == 0 {
                    spans.push(String::new())
                } else {
                    let mut current_span_str = current_span_str.to_string();
                    current_span_str.remove(offset);
                    spans.push(current_span_str);
                }
            } else {
                spans.push(current_span_str.to_string());
            }

            // Complete based on the last span
            if is_last_span {
                // Create a new span
                let new_span = Span::new(span.start, span.end - 1);

                // Parses the prefix. Completion should look up to the cursor position, not after.
                let index = pos - span.start;
                let prefix = &current_span[..index];

                if let Expr::AttributeBlock(ab) = &element_expression.expr {
                    let last_attr = ab.attributes.last().expect("at least one attribute");
                    if let Expr::Garbage = last_attr.expr.expr {
                        return self.process_completion(
                            &mut AttributeCompletion,
                            &working_set,
                            prefix,
                            new_span,
                            fake_offset,
                            pos,
                        );
                    } else {
                        return self.process_completion(
                            &mut AttributableCompletion,
                            &working_set,
                            prefix,
                            new_span,
                            fake_offset,
                            pos,
                        );
                    }
                }

                // Flags completion
                if prefix.starts_with(b"-") {
                    // Try to complete flag internally
                    let mut completer = FlagCompletion::new(element_expression.clone());
                    let result = self.process_completion(
                        &mut completer,
                        &working_set,
                        prefix,
                        new_span,
                        fake_offset,
                        pos,
                    );

                    if !result.is_empty() {
                        return result;
                    }

                    // We got no results for internal completion
                    // now we can check if external completer is set and use it
                    if let Some(closure) = config.completions.external.completer.as_ref() {
                        if let Some(external_result) =
                            self.external_completion(closure, &spans, fake_offset, new_span)
                        {
                            return external_result;
                        }
                    }
                }

                // specially check if it is currently empty - always complete commands
                if (is_passthrough_command && flat_idx == 1)
                    || (flat_idx == 0 && working_set.get_span_contents(new_span).is_empty())
                {
                    let mut completer = CommandCompletion::new(
                        flattened.clone(),
                        // flat_idx,
                        FlatShape::String,
                        true,
                    );
                    return self.process_completion(
                        &mut completer,
                        &working_set,
                        prefix,
                        new_span,
                        fake_offset,
                        pos,
                    );
                }

                // Completions that depends on the previous expression (e.g: use, source-env)
                if (is_passthrough_command && flat_idx > 1) || flat_idx > 0 {
                    if let Some(previous_expr) = flattened.get(flat_idx - 1) {
                        // Read the content for the previous expression
                        let prev_expr_str = working_set.get_span_contents(previous_expr.0).to_vec();

                        // Completion for .nu files
                        if prev_expr_str == b"use"
                            || prev_expr_str == b"overlay use"
                            || prev_expr_str == b"source-env"
                        {
                            let mut completer = DotNuCompletion::new();

                            return self.process_completion(
                                &mut completer,
                                &working_set,
                                prefix,
                                new_span,
                                fake_offset,
                                pos,
                            );
                        } else if prev_expr_str == b"ls" {
                            let mut completer = FileCompletion::new();

                            return self.process_completion(
                                &mut completer,
                                &working_set,
                                prefix,
                                new_span,
                                fake_offset,
                                pos,
                            );
                        } else if matches!(
                            previous_expr.1,
                            FlatShape::Float
                                | FlatShape::Int
                                | FlatShape::String
                                | FlatShape::List
                                | FlatShape::Bool
                                | FlatShape::Variable(_)
                        ) {
                            let mut completer = OperatorCompletion::new(element_expression.clone());

                            let operator_suggestion = self.process_completion(
                                &mut completer,
                                &working_set,
                                prefix,
                                new_span,
                                fake_offset,
                                pos,
                            );
                            if !operator_suggestion.is_empty() {
                                return operator_suggestion;
                            }
                        }
                    }
                }

                // Match other types
                match shape {
                    FlatShape::Custom(decl_id) => {
                        let mut completer = CustomCompletion::new(
                            self.stack.clone(),
                            *decl_id,
                            initial_line,
                            FileCompletion::new(),
                        );

                        return self.process_completion(
                            &mut completer,
                            &working_set,
                            prefix,
                            new_span,
                            fake_offset,
                            pos,
                        );
                    }
                    FlatShape::Directory => {
                        let mut completer = DirectoryCompletion::new();

                        return self.process_completion(
                            &mut completer,
                            &working_set,
                            prefix,
                            new_span,
                            fake_offset,
                            pos,
                        );
                    }
                    FlatShape::Filepath | FlatShape::GlobPattern => {
                        let mut completer = FileCompletion::new();

                        return self.process_completion(
                            &mut completer,
                            &working_set,
                            prefix,
                            new_span,
                            fake_offset,
                            pos,
                        );
                    }
                    flat_shape => {
                        let mut completer = CommandCompletion::new(
                            flattened.clone(),
                            // flat_idx,
                            flat_shape.clone(),
                            false,
                        );

                        let mut out: Vec<_> = self.process_completion(
                            &mut completer,
                            &working_set,
                            prefix,
                            new_span,
                            fake_offset,
                            pos,
                        );

                        if !out.is_empty() {
                            return out;
                        }

                        // Try to complete using an external completer (if set)
                        if let Some(closure) = config.completions.external.completer.as_ref() {
                            if let Some(external_result) =
                                self.external_completion(closure, &spans, fake_offset, new_span)
                            {
                                return external_result;
                            }
                        }

                        // Check for file completion
                        let mut completer = FileCompletion::new();
                        out = self.process_completion(
                            &mut completer,
                            &working_set,
                            prefix,
                            new_span,
                            fake_offset,
                            pos,
                        );

                        if !out.is_empty() {
                            return out;
                        }
                    }
                };
            }
        }

        vec![]
    }
}

impl ReedlineCompleter for NuCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        self.completion_helper(line, pos)
            .into_iter()
            .map(|s| s.suggestion)
            .collect()
    }
}

pub fn map_value_completions<'a>(
    list: impl Iterator<Item = &'a Value>,
    span: Span,
    offset: usize,
) -> Vec<SemanticSuggestion> {
    list.filter_map(move |x| {
        // Match for string values
        if let Ok(s) = x.coerce_string() {
            return Some(SemanticSuggestion {
                suggestion: Suggestion {
                    value: s,
                    span: reedline::Span {
                        start: span.start - offset,
                        end: span.end - offset,
                    },
                    ..Suggestion::default()
                },
                kind: Some(SuggestionKind::Type(x.get_type())),
            });
        }

        // Match for record values
        if let Ok(record) = x.as_record() {
            let mut suggestion = Suggestion {
                value: String::from(""), // Initialize with empty string
                span: reedline::Span {
                    start: span.start - offset,
                    end: span.end - offset,
                },
                ..Suggestion::default()
            };

            // Iterate the cols looking for `value` and `description`
            record.iter().for_each(|it| {
                // Match `value` column
                if it.0 == "value" {
                    // Convert the value to string
                    if let Ok(val_str) = it.1.coerce_string() {
                        // Update the suggestion value
                        suggestion.value = val_str;
                    }
                }

                // Match `description` column
                if it.0 == "description" {
                    // Convert the value to string
                    if let Ok(desc_str) = it.1.coerce_string() {
                        // Update the suggestion value
                        suggestion.description = Some(desc_str);
                    }
                }

                // Match `style` column
                if it.0 == "style" {
                    // Convert the value to string
                    suggestion.style = match it.1 {
                        Value::String { val, .. } => Some(lookup_ansi_color_style(val)),
                        Value::Record { .. } => Some(color_record_to_nustyle(it.1)),
                        _ => None,
                    };
                }
            });

            return Some(SemanticSuggestion {
                suggestion,
                kind: Some(SuggestionKind::Type(x.get_type())),
            });
        }

        None
    })
    .collect()
}

#[cfg(test)]
mod completer_tests {
    use super::*;

    #[test]
    fn test_completion_helper() {
        let mut engine_state =
            nu_command::add_shell_command_context(nu_cmd_lang::create_default_context());

        // Custom additions
        let delta = {
            let working_set = nu_protocol::engine::StateWorkingSet::new(&engine_state);
            working_set.render()
        };

        let result = engine_state.merge_delta(delta);
        assert!(
            result.is_ok(),
            "Error merging delta: {:?}",
            result.err().unwrap()
        );

        let mut completer = NuCompleter::new(engine_state.into(), Arc::new(Stack::new()));
        let dataset = [
            ("1 bit-sh", true, "b", vec!["bit-shl", "bit-shr"]),
            ("1.0 bit-sh", false, "b", vec![]),
            ("1 m", true, "m", vec!["mod"]),
            ("1.0 m", true, "m", vec!["mod"]),
            ("\"a\" s", true, "s", vec!["starts-with"]),
            ("sudo", false, "", Vec::new()),
            ("sudo l", true, "l", vec!["ls", "let", "lines", "loop"]),
            (" sudo", false, "", Vec::new()),
            (" sudo le", true, "le", vec!["let", "length"]),
            (
                "ls | c",
                true,
                "c",
                vec!["cd", "config", "const", "cp", "cal"],
            ),
            ("ls | sudo m", true, "m", vec!["mv", "mut", "move"]),
        ];
        for (line, has_result, begins_with, expected_values) in dataset {
            let result = completer.completion_helper(line, line.len());
            // Test whether the result is empty or not
            assert_eq!(!result.is_empty(), has_result, "line: {}", line);

            // Test whether the result begins with the expected value
            result
                .iter()
                .for_each(|x| assert!(x.suggestion.value.starts_with(begins_with)));

            // Test whether the result contains all the expected values
            assert_eq!(
                result
                    .iter()
                    .map(|x| expected_values.contains(&x.suggestion.value.as_str()))
                    .filter(|x| *x)
                    .count(),
                expected_values.len(),
                "line: {}",
                line
            );
        }
    }
}
