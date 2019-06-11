// Copyright 2019 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

//! https://www.sqlite.org/sqllogictest/doc/trunk/about.wiki

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::ops;
use std::path::Path;
use std::str::FromStr;

use failure::{bail, format_err, ResultExt};
use futures::stream::Stream;
use lazy_static::lazy_static;
use regex::Regex;
use uuid::Uuid;

use materialize::dataflow;
use materialize::glue::*;
use materialize::repr::{ColumnType, Datum};
use materialize::sql::Planner;
use sqlparser::dialect::AnsiSqlDialect;
use sqlparser::sqlparser::Parser;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Text,
    Integer,
    Real,
    Bytes,
    Oid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sort {
    No,
    Row,
    Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output<'a> {
    Values(Vec<&'a str>),
    Hashed { num_values: usize, md5: &'a str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryOutput<'a> {
    types: Vec<Type>,
    sort: Sort,
    label: Option<&'a str>,
    column_names: Option<Vec<&'a str>>,
    output: Output<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record<'a> {
    Statement {
        should_run: bool,
        rows_inserted: Option<usize>,
        sql: &'a str,
    },
    Query {
        sql: &'a str,
        output: Result<QueryOutput<'a>, &'a str>,
    },
    HashThreshold {
        threshold: u64,
    },
    Skip,
    Halt,
}

fn split_at<'a>(input: &mut &'a str, sep: &Regex) -> Result<&'a str, failure::Error> {
    match sep.find(input) {
        Some(found) => {
            let result = &input[..found.start()];
            *input = &input[found.end()..];
            Ok(result)
        }
        None => bail!("Couldn't split {:?} at {:?}", input, sep),
    }
}

fn parse_types(input: &str) -> Result<Vec<Type>, failure::Error> {
    input
        .chars()
        .map(|char| {
            Ok(match char {
                'T' => Type::Text,
                'I' => Type::Integer,
                'R' => Type::Real,
                'B' => Type::Bytes,
                'O' => Type::Oid,
                _ => bail!("Unexpected type char {} in: {}", char, input),
            })
        })
        .collect()
}

fn parse_sql<'a>(input: &mut &'a str) -> Result<&'a str, failure::Error> {
    lazy_static! {
        static ref QUERY_OUTPUT_REGEX: Regex = Regex::new("(\r?\n----\r?\n?)|$").unwrap();
    }
    split_at(input, &QUERY_OUTPUT_REGEX)
}

pub fn parse_record(mut input: &str) -> Result<Option<Record>, failure::Error> {
    if input == "" {
        // must have just been a bunch of comments
        return Ok(None);
    }

    lazy_static! {
        static ref COMMENT_AND_LINE_REGEX: Regex = Regex::new("(#[^\n]*)?\r?(\n|$)").unwrap();
    }
    let first_line = split_at(&mut input, &COMMENT_AND_LINE_REGEX)?.trim();

    if first_line == "" {
        // query starts on the next line
        return parse_record(input);
    }

    let mut words = first_line.split(' ').peekable();
    match words.next().unwrap() {
        "statement" => {
            let (should_run, rows_inserted) = match words.next() {
                Some("count") => (
                    true,
                    Some(
                        words
                            .next()
                            .ok_or_else(|| format_err!("missing insert count"))?
                            .parse::<usize>()
                            .map_err(|err| format_err!("parsing insert count: {}", err))?,
                    ),
                ),
                Some("ok") | Some("OK") => (true, None),
                Some("error") => (false, None),
                Some(other) => bail!("invalid should_run in: {}", other),
                None => (true, None),
            };
            let sql = parse_sql(&mut input)?;
            if input != "" {
                bail!("leftover input: {}", input)
            }
            Ok(Some(Record::Statement {
                should_run,
                rows_inserted,
                sql,
            }))
        }
        "query" => {
            if words.peek() == Some(&"error") {
                let error = &first_line[12..]; // everything after "query error "
                let sql = input;
                Ok(Some(Record::Query {
                    sql,
                    output: Err(error),
                }))
            } else {
                let types = parse_types(
                    words
                        .next()
                        .ok_or_else(|| format_err!("missing types in: {}", first_line))?,
                )?;
                let mut sort = Sort::No;
                let mut check_column_names = false;
                if let Some(options) = words.next() {
                    for option in options.split(',') {
                        match option {
                            "nosort" => sort = Sort::No,
                            "rowsort" => sort = Sort::Row,
                            "valuesort" => sort = Sort::Value,
                            "colnames" => check_column_names = true,
                            other => {
                                if other.starts_with("partialsort") {
                                    // TODO(jamii) https://github.com/cockroachdb/cockroach/blob/d2f7fbf5dd1fc1a099bbad790a2e1f7c60a66cc3/pkg/sql/logictest/logic.go#L153
                                    // partialsort has comma-separated arguments so our parsing is totally broken
                                    // luckily it always comes last in the existing tests, so we can just bail out for now
                                    sort = Sort::Row;
                                    break;
                                } else {
                                    bail!("Unrecognized option {:?} in {:?}", other, options);
                                }
                            }
                        };
                    }
                }
                let label = words.next();
                let sql = parse_sql(&mut input)?;
                lazy_static! {
                    static ref LINE_REGEX: Regex = Regex::new("\r?(\n|$)").unwrap();
                    static ref HASH_REGEX: Regex =
                        Regex::new(r"(\S+) values hashing to (\S+)").unwrap();
                }
                let column_names = if check_column_names {
                    Some(
                        split_at(&mut input, &LINE_REGEX)?
                            .split(' ')
                            .filter(|s| !s.is_empty())
                            .collect(),
                    )
                } else {
                    None
                };
                let output = match HASH_REGEX.captures(input) {
                    Some(captures) => Output::Hashed {
                        num_values: captures.get(1).unwrap().as_str().parse::<usize>()?,
                        md5: captures.get(2).unwrap().as_str(),
                    },
                    None => {
                        lazy_static! {
                            static ref WHITESPACE_REGEX: Regex = Regex::new(r"\s+").unwrap();
                        }
                        Output::Values(WHITESPACE_REGEX.split(input.trim()).collect())
                    }
                };
                Ok(Some(Record::Query {
                    sql,
                    output: Ok(QueryOutput {
                        types,
                        sort,
                        label,
                        column_names,
                        output,
                    }),
                }))
            }
        }
        "hash-threshold" => {
            let threshold = words
                .next()
                .ok_or_else(|| format_err!("missing threshold in: {}", first_line))?
                .parse::<u64>()
                .map_err(|err| format_err!("invalid threshold ({}) in: {}", err, first_line))?;
            if input != "" {
                bail!("leftover input: {}", input)
            }
            Ok(Some(Record::HashThreshold { threshold }))
        }

        // we'll follow the postgresql version of all these tests
        "skipif" => {
            match words.next().unwrap() {
                "postgresql" => Ok(None),
                _ => {
                    // query starts on the next line
                    parse_record(input)
                }
            }
        }
        "onlyif" => {
            match words.next().unwrap() {
                "postgresql" => {
                    // query starts on the next line
                    parse_record(input)
                }
                _ => Ok(None),
            }
        }

        "halt" => Ok(Some(Record::Halt)),

        // this is some cockroach-specific thing, we don't care
        "subtest" | "user" | "kv-batch-size" => Ok(None),

        other => bail!("Unexpected start of record: {}", other),
    }
}

pub fn parse_records(input: &str) -> impl Iterator<Item = Result<Record, failure::Error>> {
    lazy_static! {
        static ref DOUBLE_LINE_REGEX: Regex = Regex::new("(\n|\r\n)(\n|\r\n)").unwrap();
    }
    DOUBLE_LINE_REGEX
        .split(input)
        .map(str::trim)
        .filter(|lines| *lines != "")
        .filter_map(|lines| parse_record(lines).transpose())
        .take_while(|record| match record {
            Ok(Record::Halt) => false,
            _ => true,
        })
}

#[derive(Debug)]
pub enum Outcome<'a> {
    Unsupported {
        error: failure::Error,
    },
    ParseFailure {
        error: sqlparser::sqlparser::ParserError,
    },
    PlanFailure {
        error: failure::Error,
    },
    UnexpectedPlanSuccess {
        expected_error: &'a str,
    },
    WrongNumberOfRowsInserted {
        expected_rows_inserted: usize,
        actual_response: SqlResponse,
    },
    InferenceFailure {
        expected_types: &'a [Type],
        inferred_types: Vec<ColumnType>,
    },
    WrongColumnNames {
        expected_column_names: &'a Vec<&'a str>,
        inferred_column_names: Vec<Option<String>>,
    },
    OutputFailure {
        expected_output: &'a Output<'a>,
        actual_output: Vec<Vec<Datum>>,
    },
    Bail {
        cause: Box<Outcome<'a>>,
    },
    Success,
}

const NUM_OUTCOMES: usize = 10;

impl<'a> Outcome<'a> {
    fn code(&self) -> usize {
        match self {
            Outcome::Unsupported { .. } => 0,
            Outcome::ParseFailure { .. } => 1,
            Outcome::PlanFailure { .. } => 2,
            Outcome::UnexpectedPlanSuccess { .. } => 3,
            Outcome::WrongNumberOfRowsInserted { .. } => 4,
            Outcome::InferenceFailure { .. } => 5,
            Outcome::WrongColumnNames { .. } => 6,
            Outcome::OutputFailure { .. } => 7,
            Outcome::Bail { .. } => 8,
            Outcome::Success => 9,
        }
    }
}
#[derive(Default, Debug, Eq, PartialEq)]
pub struct Outcomes([usize; NUM_OUTCOMES]);

impl ops::AddAssign<Outcomes> for Outcomes {
    fn add_assign(&mut self, rhs: Outcomes) {
        for (lhs, rhs) in self.0.iter_mut().zip(rhs.0.iter()) {
            *lhs += rhs
        }
    }
}

impl FromStr for Outcomes {
    type Err = failure::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let pieces: Vec<_> = s.split(',').collect();
        if pieces.len() != NUM_OUTCOMES {
            bail!(
                "expected-outcomes argument needs {} comma-separated ints",
                NUM_OUTCOMES
            );
        }
        Ok(Outcomes([
            pieces[0].parse()?,
            pieces[1].parse()?,
            pieces[2].parse()?,
            pieces[3].parse()?,
            pieces[4].parse()?,
            pieces[5].parse()?,
            pieces[6].parse()?,
            pieces[7].parse()?,
            pieces[8].parse()?,
            pieces[9].parse()?,
        ]))
    }
}

impl fmt::Display for Outcomes {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "unsupported={} parse-failure={} plan-failure={} unexpected-plan-success={} wrong-number-of-rows-inserted={} inference-failure={} wrong-column-names={} output-failure={} bail={} success={} total={}",
            self.0[0],
            self.0[1],
            self.0[2],
            self.0[3],
            self.0[4],
            self.0[5],
            self.0[6],
            self.0[7],
            self.0[8],
            self.0[9],
            self.0.iter().sum::<usize>(),
        )
    }
}

impl Outcomes {
    pub fn any_failed(&self) -> bool {
        self.0[0] + self.0[6] < self.0.iter().sum::<usize>()
    }
}

trait RecordRunner {
    fn run_record<'a>(&mut self, record: &'a Record) -> Result<Outcome<'a>, failure::Error>;
}

const NUM_TIMELY_WORKERS: usize = 3;

struct FullState {
    // clock: Clock,
    timer: std::time::Instant,
    planner: Planner,
    dataflow_command_senders: Vec<UnboundedSender<(DataflowCommand, CommandMeta)>>,
    threads: Vec<std::thread::Thread>,
    // this is only here to avoid dropping it too early
    _dataflow_workers: Box<Drop>,
    peek_results_mux: PeekResultsMux,
}

fn format_row(row: &[Datum], types: &[Type]) -> Vec<String> {
    types
        .iter()
        .zip(row.iter())
        .map(|(typ, datum)| match (typ, datum) {
            // the documented formatting rules in https://www.sqlite.org/sqllogictest/doc/trunk/about.wiki
            (_, Datum::Null) => "NULL".to_owned(),
            (Type::Integer, Datum::Int64(i)) => format!("{}", i),
            (Type::Real, Datum::Float64(f)) => format!("{:.3}", f),
            (Type::Text, Datum::String(string)) => {
                if string.is_empty() {
                    "(empty)".to_owned()
                } else {
                    string.to_owned()
                }
            }

            // weird type coercions that sqllogictest doesn't document
            (Type::Integer, Datum::Float64(f)) => format!("{:.0}", f.trunc()),
            (Type::Integer, Datum::String(_)) => "0".to_owned(),
            (Type::Integer, Datum::False) => "0".to_owned(),
            (Type::Integer, Datum::True) => "1".to_owned(),
            (Type::Text, Datum::Int64(i)) => format!("{}", i),
            (Type::Text, Datum::Float64(f)) => format!("{:.3}", f),

            other => panic!("Don't know how to format {:?}", other),
        })
        .collect()
}

impl FullState {
    fn start() -> Self {
        let planner = Planner::default();
        let (dataflow_command_senders, dataflow_command_receivers) =
            (0..NUM_TIMELY_WORKERS).map(|_| unbounded()).unzip();
        let peek_results_mux = PeekResultsMux::default();
        let dataflow_workers = dataflow::serve(
            dataflow_command_receivers,
            dataflow::PeekResultsHandler::Local(peek_results_mux.clone()),
            NUM_TIMELY_WORKERS,
        )
        .unwrap();

        let threads = dataflow_workers
            .guards()
            .iter()
            .map(|jh| jh.thread().clone())
            .collect::<Vec<_>>();

        FullState {
            // clock,
            timer: std::time::Instant::now(),
            planner,
            dataflow_command_senders,
            _dataflow_workers: Box::new(dataflow_workers),
            peek_results_mux,
            threads,
        }
    }

    fn send_dataflow_command(
        &self,
        dataflow_command: DataflowCommand,
    ) -> UnboundedReceiver<PeekResults> {
        let timestamp = self.timer.elapsed().as_millis() as u64;
        let uuid = Uuid::new_v4();
        let receiver = self
            .peek_results_mux
            .write()
            .unwrap()
            .channel(uuid)
            .unwrap();
        for (index, dataflow_command_sender) in self.dataflow_command_senders.iter().enumerate() {
            dataflow_command_sender
                .unbounded_send((
                    dataflow_command.clone(),
                    CommandMeta {
                        connection_uuid: uuid,
                        timestamp: Some(timestamp),
                    },
                ))
                .unwrap();

            self.threads[index].unpark();
        }
        receiver
    }

    fn receive_peek_results(&self, receiver: UnboundedReceiver<PeekResults>) -> Vec<Vec<Datum>> {
        let mut results = vec![];
        let mut receiver = receiver.wait();
        for _ in 0..NUM_TIMELY_WORKERS {
            results.append(&mut receiver.next().unwrap().unwrap());
        }
        results
    }
}

impl RecordRunner for FullState {
    fn run_record<'a>(&mut self, record: &'a Record) -> Result<Outcome<'a>, failure::Error> {
        match &record {
            Record::Statement {
                should_run,
                rows_inserted: expected_rows_inserted,
                sql,
            } => {
                lazy_static! {
                    static ref INDEX_STATEMENT_REGEX: Regex =
                        Regex::new("^(CREATE (UNIQUE )?INDEX|DROP INDEX|REINDEX)").unwrap();
                }
                if INDEX_STATEMENT_REGEX.is_match(sql) {
                    // sure, we totally made you an index...
                    return Ok(Outcome::Success);
                }

                // we don't support non-materialized views
                let sql = sql.replace("CREATE VIEW", "CREATE MATERIALIZED VIEW");

                if let Err(error) = Parser::parse_sql(&AnsiSqlDialect {}, sql.to_string()) {
                    if *should_run {
                        return Ok(Outcome::ParseFailure { error });
                    } else {
                        return Ok(Outcome::Success);
                    }
                }

                let dataflow_command = match self.planner.handle_command(sql.to_string()) {
                    Ok((response, dataflow_command)) => {
                        if let Some(expected_rows_inserted) = *expected_rows_inserted {
                            match response {
                                SqlResponse::Inserted(actual_rows_inserted)
                                    if actual_rows_inserted == expected_rows_inserted => {}
                                _ => {
                                    return Ok(Outcome::WrongNumberOfRowsInserted {
                                        expected_rows_inserted,
                                        actual_response: response,
                                    });
                                }
                            }
                        }
                        dataflow_command
                    }
                    Err(error) => {
                        if *should_run {
                            return Ok(Outcome::PlanFailure { error });
                        } else {
                            return Ok(Outcome::Success);
                        }
                    }
                };
                let _receiver = self.send_dataflow_command(dataflow_command.unwrap());

                Ok(Outcome::Success)
            }
            Record::Query { sql, output } => {
                if let Err(error) = Parser::parse_sql(&AnsiSqlDialect {}, sql.to_string()) {
                    return Ok(Outcome::ParseFailure { error });
                }

                let (typ, dataflow_command) = match self.planner.handle_command(sql.to_string()) {
                    Ok((SqlResponse::Peeking { typ }, dataflow_command)) => (typ, dataflow_command),
                    Ok(other) => {
                        return Ok(Outcome::PlanFailure {
                            error: failure::format_err!(
                                "Query did not result in Peeking, instead got {:?}",
                                other
                            ),
                        });
                    }
                    Err(error) => {
                        // TODO(jamii) check error messages, once ours stabilize
                        if output.is_err() {
                            return Ok(Outcome::Success);
                        } else {
                            let error_string = format!("{}", error);
                            if error_string.contains("supported")
                                || error_string.contains("overload")
                            {
                                // this is a failure, but it's caused by lack of support rather than by bugs
                                return Ok(Outcome::Unsupported { error });
                            } else {
                                return Ok(Outcome::PlanFailure { error });
                            }
                        }
                    }
                };

                match output {
                    Err(expected_error) => Ok(Outcome::UnexpectedPlanSuccess { expected_error }),
                    Ok(QueryOutput {
                        sort,
                        types: expected_types,
                        column_names: expected_column_names,
                        output: expected_output,
                        ..
                    }) => {
                        let inferred_types = &typ.column_types;
                        // sqllogictest coerces the output into the expected type, so expected_type is often wrong :(
                        // but at least it will be the correct length
                        if inferred_types.len() != expected_types.len() {
                            return Ok(Outcome::InferenceFailure {
                                expected_types,
                                inferred_types: inferred_types.to_vec(),
                            });
                        }

                        if let Some(expected_column_names) = expected_column_names {
                            let inferred_column_names = typ
                                .column_types
                                .iter()
                                .map(|t| t.name.clone())
                                .collect::<Vec<_>>();
                            if expected_column_names
                                .iter()
                                .map(|s| Some(&**s))
                                .collect::<Vec<_>>()
                                != inferred_column_names
                                    .iter()
                                    .map(|n| n.as_ref().map(|s| &**s))
                                    .collect::<Vec<_>>()
                            {
                                return Ok(Outcome::WrongColumnNames {
                                    expected_column_names,
                                    inferred_column_names,
                                });
                            }
                        }

                        let receiver = self.send_dataflow_command(dataflow_command.unwrap());
                        let results = self.receive_peek_results(receiver);

                        let mut rows = results
                            .iter()
                            .map(|row| format_row(row, &**expected_types))
                            .collect::<Vec<_>>();
                        if let Sort::Row = sort {
                            rows.sort();
                        }
                        let mut values = rows.into_iter().flat_map(|row| row).collect::<Vec<_>>();
                        if let Sort::Value = sort {
                            values.sort();
                        }

                        match expected_output {
                            Output::Values(expected_values) => {
                                if values != *expected_values {
                                    dbg!(&record);
                                    return Ok(Outcome::OutputFailure {
                                        expected_output,
                                        actual_output: results,
                                    });
                                }
                            }
                            Output::Hashed {
                                num_values,
                                md5: expected_md5,
                            } => {
                                let mut md5_context = md5::Context::new();
                                for value in &values {
                                    md5_context.consume(value);
                                    md5_context.consume("\n");
                                }
                                let md5 = format!("{:x}", md5_context.compute());
                                if values.len() != *num_values || md5 != *expected_md5 {
                                    return Ok(Outcome::OutputFailure {
                                        expected_output,
                                        actual_output: results,
                                    });
                                }
                            }
                        }

                        Ok(Outcome::Success)
                    }
                }
            }
            _ => Ok(Outcome::Success),
        }
    }
}

impl Drop for FullState {
    fn drop(&mut self) {
        for (index, dataflow_command_sender) in self.dataflow_command_senders.iter().enumerate() {
            drop(dataflow_command_sender.unbounded_send((
                DataflowCommand::Shutdown,
                CommandMeta {
                    connection_uuid: Uuid::nil(),
                    timestamp: None,
                },
            )));

            self.threads[index].unpark();
        }
    }
}

struct OnlyParseState;

impl OnlyParseState {
    fn start() -> Self {
        OnlyParseState
    }
}

impl RecordRunner for OnlyParseState {
    fn run_record<'a>(&mut self, record: &'a Record) -> Result<Outcome<'a>, failure::Error> {
        match &record {
            Record::Statement {
                should_run, sql, ..
            } => {
                lazy_static! {
                    static ref INDEX_STATEMENT_REGEX: Regex =
                        Regex::new("^(CREATE (UNIQUE )?INDEX|DROP INDEX|REINDEX)").unwrap();
                }
                if INDEX_STATEMENT_REGEX.is_match(sql) {
                    // sure, we totally made you an index...
                    return Ok(Outcome::Success);
                }

                // we don't support non-materialized views
                let sql = sql.replace("CREATE VIEW", "CREATE MATERIALIZED VIEW");

                if let Err(error) = Parser::parse_sql(&AnsiSqlDialect {}, sql.to_string()) {
                    if *should_run {
                        return Ok(Outcome::ParseFailure { error });
                    } else {
                        return Ok(Outcome::Success);
                    }
                }
                Ok(Outcome::Success)
            }
            Record::Query { sql, .. } => {
                if let Err(error) = Parser::parse_sql(&AnsiSqlDialect {}, sql.to_string()) {
                    return Ok(Outcome::ParseFailure { error });
                }
                Ok(Outcome::Success)
            }
            _ => Ok(Outcome::Success),
        }
    }
}

pub fn run_string(source: &str, input: &str, verbosity: usize, only_parse: bool) -> Outcomes {
    let mut outcomes = Outcomes::default();
    let mut state: Box<RecordRunner> = if only_parse {
        Box::new(OnlyParseState::start())
    } else {
        Box::new(FullState::start())
    };
    if verbosity >= 1 {
        println!("==> {}", source);
    }
    for record in parse_records(&input) {
        let record = record.unwrap();

        if verbosity >= 3 {
            match &record {
                Record::Statement { sql, .. } => println!("{}", sql),
                Record::Query { sql, .. } => println!("{}", sql),
                _ => (),
            }
        }

        let mut outcome = state
            .run_record(&record)
            .with_context(|err| format!("In {}:\n{}", source, err))
            .unwrap();

        // if we failed to execute a statement, running the rest of the tests in this file will probably cause false positives
        match (&record, &outcome) {
            (_, Outcome::Success) => (),
            (Record::Statement { sql, .. }, _) if !sql.contains("CREATE VIEW") => {
                outcome = Outcome::Bail {
                    cause: Box::new(outcome),
                };
            }
            _ => (),
        }

        // print failures in verbose mode
        match &outcome {
            Outcome::Success => (),
            _ => {
                if verbosity >= 2 {
                    if verbosity < 3 {
                        match &record {
                            Record::Statement { sql, .. } => println!("{}", sql),
                            Record::Query { sql, .. } => println!("{}", sql),
                            _ => (),
                        }
                    }
                    println!("{:?}", outcome);
                    println!("In {}", source);
                }
            }
        }

        outcomes.0[outcome.code()] += 1;

        if let Outcome::Bail { .. } = outcome {
            break;
        }
    }
    outcomes
}

pub fn run_file(filename: &Path, verbosity: usize, only_parse: bool) -> Outcomes {
    let mut input = String::new();
    File::open(filename)
        .unwrap()
        .read_to_string(&mut input)
        .unwrap();
    run_string(
        &format!("{}", filename.display()),
        &input,
        verbosity,
        only_parse,
    )
}

pub fn run_stdin(verbosity: usize, only_parse: bool) -> Outcomes {
    let mut input = String::new();
    std::io::stdin().lock().read_to_string(&mut input).unwrap();
    run_string("<stdin>", &input, verbosity, only_parse)
}

pub fn fuzz(sqls: &str) {
    let mut state = FullState::start();
    for sql in sqls.split(';') {
        if let Ok((sql_response, dataflow_command)) = state.planner.handle_command(sql.to_owned()) {
            if let Some(dataflow_command) = dataflow_command {
                let receiver = state.send_dataflow_command(dataflow_command);
                if let SqlResponse::Peeking { typ } = sql_response {
                    for row in state.receive_peek_results(receiver) {
                        for (typ, datum) in typ.column_types.iter().zip(row.into_iter()) {
                            assert!(
                                (typ.scalar_type == datum.scalar_type())
                                    || (typ.nullable && datum.is_null()),
                                "{:?} was inferred to have type {:?}",
                                typ.scalar_type,
                                datum.scalar_type(),
                            );
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::fs::File;
    use std::io::Read;

    use walkdir::WalkDir;

    #[test]
    fn fuzz_artifacts() {
        let mut input = String::new();
        for entry in WalkDir::new("../../fuzz/artifacts/fuzz_sqllogictest/") {
            let entry = entry.unwrap();
            if entry.path().is_file() {
                input.clear();
                File::open(&entry.path())
                    .unwrap()
                    .read_to_string(&mut input)
                    .unwrap();
                fuzz(&input);
            }
        }
    }
}
