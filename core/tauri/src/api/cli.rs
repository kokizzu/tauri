// Copyright 2019-2021 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use crate::api::config::{CliArg, CliConfig};

use clap::{
  crate_authors, crate_description, crate_name, crate_version, App, Arg, ArgMatches, ErrorKind,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

#[macro_use]
mod macros;

/// The resolution of a arg match.
#[derive(Default, Debug, Serialize)]
#[non_exhaustive]
pub struct ArgData {
  /// The value of the arg.
  /// - Value::Bool if it's a flag,
  /// - Value::Array if it's multiple,
  /// - Value::String if it has value,
  /// - Value::Null otherwise.
  pub value: Value,
  /// The number of occurrences of the arg.
  /// e.g. `./app --arg 1 --arg 2 --arg 2 3 4` results in three occurrences.
  pub occurrences: u64,
}

/// The matched subcommand.
#[derive(Default, Debug, Serialize)]
#[non_exhaustive]
pub struct SubcommandMatches {
  /// The subcommand name.
  pub name: String,
  /// The subcommand arg matches.
  pub matches: Matches,
}

/// The arg matches of a command.
#[derive(Default, Debug, Serialize)]
#[non_exhaustive]
pub struct Matches {
  /// Data structure mapping each found arg with its resolution.
  pub args: HashMap<String, ArgData>,
  /// The matched subcommand if found.
  pub subcommand: Option<Box<SubcommandMatches>>,
}

impl Matches {
  /// Set a arg match.
  pub(crate) fn set_arg(&mut self, name: String, value: ArgData) {
    self.args.insert(name, value);
  }

  /// Sets the subcommand matches.
  pub(crate) fn set_subcommand(&mut self, name: String, matches: Matches) {
    self.subcommand = Some(Box::new(SubcommandMatches { name, matches }));
  }
}

/// Gets the arg matches of the CLI definition.
pub fn get_matches(cli: &CliConfig) -> crate::api::Result<Matches> {
  let about = cli
    .description()
    .unwrap_or(&crate_description!().to_string())
    .to_string();
  let app = get_app(crate_name!(), Some(&about), cli);
  match app.try_get_matches() {
    Ok(matches) => Ok(get_matches_internal(cli, &matches)),
    Err(e) => match e.kind {
      ErrorKind::DisplayHelp => {
        let mut matches = Matches::default();
        let help_text = e.to_string();
        matches.args.insert(
          "help".to_string(),
          ArgData {
            value: Value::String(help_text),
            occurrences: 0,
          },
        );
        Ok(matches)
      }
      ErrorKind::DisplayVersion => {
        let mut matches = Matches::default();
        matches
          .args
          .insert("version".to_string(), Default::default());
        Ok(matches)
      }
      _ => Err(e.into()),
    },
  }
}

fn get_matches_internal(config: &CliConfig, matches: &ArgMatches) -> Matches {
  let mut cli_matches = Matches::default();
  map_matches(config, matches, &mut cli_matches);

  if let Some((subcommand_name, subcommand_matches)) = matches.subcommand() {
    let mut subcommand_cli_matches = Matches::default();
    map_matches(
      config.subcommands().unwrap().get(subcommand_name).unwrap(),
      subcommand_matches,
      &mut subcommand_cli_matches,
    );
    cli_matches.set_subcommand(subcommand_name.to_string(), subcommand_cli_matches);
  }

  cli_matches
}

fn map_matches(config: &CliConfig, matches: &ArgMatches, cli_matches: &mut Matches) {
  if let Some(args) = config.args() {
    for arg in args {
      let occurrences = matches.occurrences_of(arg.name.clone());
      let value = if occurrences == 0 || !arg.takes_value.unwrap_or(false) {
        Value::Bool(occurrences > 0)
      } else if arg.multiple.unwrap_or(false) {
        matches
          .values_of(arg.name.clone())
          .map(|v| {
            let mut values = Vec::new();
            for value in v {
              values.push(Value::String(value.to_string()));
            }
            Value::Array(values)
          })
          .unwrap_or(Value::Null)
      } else {
        matches
          .value_of(arg.name.clone())
          .map(|v| Value::String(v.to_string()))
          .unwrap_or(Value::Null)
      };

      cli_matches.set_arg(arg.name.clone(), ArgData { value, occurrences });
    }
  }
}

fn get_app<'a>(name: &str, about: Option<&'a String>, config: &'a CliConfig) -> App<'a> {
  let mut app = App::new(name)
    .author(crate_authors!())
    .version(crate_version!());

  if let Some(about) = about {
    app = app.about(&**about);
  }
  if let Some(long_description) = config.long_description() {
    app = app.long_about(&**long_description);
  }
  if let Some(before_help) = config.before_help() {
    app = app.before_help(&**before_help);
  }
  if let Some(after_help) = config.after_help() {
    app = app.after_help(&**after_help);
  }

  if let Some(args) = config.args() {
    for arg in args {
      let arg_name = arg.name.as_ref();
      app = app.arg(get_arg(arg_name, arg));
    }
  }

  if let Some(subcommands) = config.subcommands() {
    for (subcommand_name, subcommand) in subcommands {
      let clap_subcommand = get_app(subcommand_name, subcommand.description(), subcommand);
      app = app.subcommand(clap_subcommand);
    }
  }

  app
}

fn get_arg<'a>(arg_name: &'a str, arg: &'a CliArg) -> Arg<'a> {
  let mut clap_arg = Arg::new(arg_name).long(arg_name);

  if let Some(short) = arg.short {
    clap_arg = clap_arg.short(short);
  }

  clap_arg = bind_string_arg!(arg, clap_arg, description, about);
  clap_arg = bind_string_arg!(arg, clap_arg, long_description, long_about);
  clap_arg = bind_value_arg!(arg, clap_arg, takes_value);
  clap_arg = bind_value_arg!(arg, clap_arg, multiple);
  clap_arg = bind_value_arg!(arg, clap_arg, multiple_occurrences);
  clap_arg = bind_value_arg!(arg, clap_arg, number_of_values);
  clap_arg = bind_string_slice_arg!(arg, clap_arg, possible_values);
  clap_arg = bind_value_arg!(arg, clap_arg, min_values);
  clap_arg = bind_value_arg!(arg, clap_arg, max_values);
  clap_arg = bind_value_arg!(arg, clap_arg, required);
  clap_arg = bind_string_arg!(
    arg,
    clap_arg,
    required_unless_present,
    required_unless_present
  );
  clap_arg = bind_string_slice_arg!(arg, clap_arg, required_unless_present_all);
  clap_arg = bind_string_slice_arg!(arg, clap_arg, required_unless_present_any);
  clap_arg = bind_string_arg!(arg, clap_arg, conflicts_with, conflicts_with);
  clap_arg = bind_string_slice_arg!(arg, clap_arg, conflicts_with_all);
  clap_arg = bind_string_arg!(arg, clap_arg, requires, requires);
  clap_arg = bind_string_slice_arg!(arg, clap_arg, requires_all);
  clap_arg = bind_if_arg!(arg, clap_arg, requires_if);
  clap_arg = bind_if_arg!(arg, clap_arg, required_if_eq);
  clap_arg = bind_value_arg!(arg, clap_arg, require_equals);
  clap_arg = bind_value_arg!(arg, clap_arg, index);

  clap_arg
}
