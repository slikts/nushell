//! A script's usage context. When a script has a usage spec, in a `.<script>.usage.kdl` file
//! beside it or else in its own `USAGE` comments, the words it is called with are parsed against
//! the spec before it runs, and the script gets the result as the constant `$usage`. The result
//! has the shape of a binding in usage's argv corpus: the selected subcommand path as `cmd`, and
//! `flags`, `args`, `clauses` and `external` keyed by the names the spec gives, each left out
//! when empty. A script without a spec runs as it does without this crate.

use nu_protocol::{
    Record, ShellError, Span, Value,
    engine::{EngineState, Stack, StateWorkingSet},
    shell_error::generic::GenericError,
};
use std::{fmt::Display, path::Path};
use usage::{
    Spec,
    error::UsageErr,
    parse::{ParseOutput, ParseValue},
};

/// What a call of a script comes to once its spec has read it.
pub enum Call {
    /// Run the script, with `$usage` bound if it has a spec.
    Run,
    /// Print this page instead of running the script, because the call asked for it.
    Answer(String),
}

/// Reads the call of `script` with `args`, the words after it, against the script's spec, and
/// binds what the call parsed to as `$usage`.
pub fn bind(
    engine_state: &mut EngineState,
    stack: &Stack,
    script: &str,
    args: &[String],
) -> Result<Call, ShellError> {
    let cwd = engine_state.cwd(Some(stack))?;
    // A path nu can't resolve or read is reported when nu runs the script.
    let Ok(script) = nu_path::absolute_with(script, cwd.as_std_path()) else {
        return Ok(Call::Run);
    };
    let Some(spec) = spec(&script)? else {
        return Ok(Call::Run);
    };

    let mut words = vec![spec.bin.clone()];
    words.extend_from_slice(args);
    let parsed = match usage::parse::parse(&spec, &words) {
        Ok(parsed) => parsed,
        Err(refusal) => {
            return match requested_page(&spec, &words) {
                Some(page) => Ok(Call::Answer(page)),
                None => Err(refused(&spec, refusal)),
            };
        }
    };

    let value = binding(&parsed);
    let mut working_set = StateWorkingSet::new(engine_state);
    let usage = working_set.add_variable(
        b"$usage".to_vec(),
        Span::unknown(),
        value.get_type(),
        false,
    );
    working_set.set_variable_const_val(usage, value);
    engine_state.merge_delta(working_set.render())?;
    Ok(Call::Run)
}

/// The spec `script` runs with, found the way `usage exec` finds it: a `.<script>.usage.kdl`
/// file beside the script, else the script's own `USAGE` comments. `None` when it has neither,
/// or can't be read.
fn spec(script: &Path) -> Result<Option<Spec>, ShellError> {
    let Some(name) = script.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    let sidecar_name = format!(".{name}.usage.kdl");
    let sidecar = script.with_file_name(&sidecar_name);
    if sidecar.exists() {
        let mut spec = Spec::parse_file(&sidecar).map_err(|error| invalid(&sidecar, error))?;
        // usage names a spec after the file it is in, and the program a sidecar describes is
        // the script, so it takes the script's name unless the sidecar gives one.
        if spec.name == sidecar_name {
            spec.name = name.to_string();
        }
        if spec.bin == sidecar_name {
            spec.bin = name.to_string();
        }
        return Ok(Some(spec));
    }

    let Ok(source) = std::fs::read_to_string(script) else {
        return Ok(None);
    };
    if !source.lines().any(starts_spec) {
        return Ok(None);
    }
    Spec::parse_script(script)
        .map(Some)
        .map_err(|error| invalid(script, error))
}

/// Whether `line` is a spec comment in a Nu script, in usage's syntax: `#USAGE …`,
/// `#[USAGE] …` or `# [USAGE] …`.
fn starts_spec(line: &str) -> bool {
    line.strip_prefix('#').is_some_and(|comment| {
        comment.starts_with("USAGE")
            || comment
                .strip_prefix(' ')
                .unwrap_or(comment)
                .starts_with("[USAGE]")
    })
}

/// The page a call asked for in place of running, such as its help. usage's parser answers
/// those requests with the page, which a failed parse reports only as text.
fn requested_page(spec: &Spec, words: &[String]) -> Option<String> {
    let parsed = usage::parse::parse_partial(spec, words).ok()?;
    parsed.errors.into_iter().find_map(|error| match error {
        UsageErr::Help(page) | UsageErr::Version(page) => Some(page),
        _ => None,
    })
}

/// `parsed`, recorded the way usage's corpus records a binding.
fn binding(parsed: &ParseOutput) -> Value {
    let span = Span::unknown();
    let mut record = Record::new();

    let cmd: Vec<Value> = parsed
        .cmds
        .iter()
        // The root command is the program itself, not part of the path.
        .skip(1)
        .map(|cmd| Value::string(cmd.name.clone(), span))
        .collect();
    if !cmd.is_empty() {
        record.push("cmd", Value::list(cmd, span));
    }

    let flags = named(parsed.flags.iter().map(|(flag, value)| (&flag.name, value)));
    if !flags.is_empty() {
        record.push("flags", Value::record(flags, span));
    }

    let args = named(parsed.args.iter().map(|(arg, value)| (&arg.name, value)));
    if !args.is_empty() {
        record.push("args", Value::record(args, span));
    }

    let clauses: Record = parsed
        .clauses
        .iter()
        .map(|(name, instances)| {
            let instances = instances
                .iter()
                .map(|instance| {
                    let instance = named(instance.iter().map(|(arg, value)| (&arg.name, value)));
                    Value::record(instance, span)
                })
                .collect();
            (name.clone(), Value::list(instances, span))
        })
        .collect();
    if !clauses.is_empty() {
        record.push("clauses", Value::record(clauses, span));
    }

    if let Some(external) = parsed.external.as_ref().filter(|words| !words.is_empty()) {
        let words = external
            .iter()
            .map(|word| Value::string(word.clone(), span))
            .collect();
        record.push("external", Value::list(words, span));
    }

    Value::record(record, span)
}

/// Bound values under the names the spec gives them.
fn named<'a>(entries: impl Iterator<Item = (&'a String, &'a ParseValue)>) -> Record {
    entries
        .map(|(name, value)| (name.clone(), bound(value)))
        .collect()
}

/// A bound value: a string or a boolean, or a list of either for what repeats.
fn bound(value: &ParseValue) -> Value {
    let span = Span::unknown();
    match value {
        ParseValue::Bool(value) => Value::bool(*value, span),
        ParseValue::String(value) => Value::string(value.clone(), span),
        ParseValue::MultiBool(values) => Value::list(
            values.iter().map(|value| Value::bool(*value, span)).collect(),
            span,
        ),
        ParseValue::MultiString(values) => Value::list(
            values
                .iter()
                .map(|value| Value::string(value.clone(), span))
                .collect(),
            span,
        ),
    }
}

/// A call the script's spec doesn't accept, reported before the script runs.
fn refused(spec: &Spec, refusal: impl Display) -> ShellError {
    GenericError::new(
        format!("{} can't be called this way", spec.bin),
        refusal.to_string(),
        Span::unknown(),
    )
    .into()
}

/// A spec usage can't read.
fn invalid(file: &Path, error: UsageErr) -> ShellError {
    GenericError::new(
        format!("Invalid usage spec in {}", file.display()),
        format!("{:?}", usage::miette::Error::from(error)),
        Span::unknown(),
    )
    .into()
}
